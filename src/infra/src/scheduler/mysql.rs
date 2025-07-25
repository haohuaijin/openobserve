// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Duration;
use config::{
    metrics::DB_QUERY_NUMS,
    utils::{hash::Sum64, time::now_micros},
};
use sqlx::Row;

use super::{
    TRIGGERS_KEY, Trigger, TriggerId, TriggerModule, TriggerStatus, get_scheduler_max_retries,
};
use crate::{
    db::{
        self, IndexStatement,
        mysql::{CLIENT, CLIENT_DDL, CLIENT_RO, create_index},
    },
    errors::{DbError, Error, Result},
};

pub struct MySqlScheduler {}

impl MySqlScheduler {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for MySqlScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl super::Scheduler for MySqlScheduler {
    /// Creates the Scheduled Jobs table
    async fn create_table(&self) -> Result<()> {
        let pool = CLIENT_DDL.clone();
        DB_QUERY_NUMS
            .with_label_values(&["create", "scheduled_jobs", ""])
            .inc();
        sqlx::query(
            r#"
CREATE TABLE IF NOT EXISTS scheduled_jobs
(
    id           BIGINT not null primary key AUTO_INCREMENT,
    org          VARCHAR(100) not null,
    module       INT not null,
    module_key   VARCHAR(256) not null,
    is_realtime  BOOLEAN default false not null,
    is_silenced  BOOLEAN default false not null,
    status       INT not null,
    start_time   BIGINT,
    end_time     BIGINT,
    retries      INT not null,
    next_run_at  BIGINT not null,
    created_at   TIMESTAMP default CURRENT_TIMESTAMP,
    data         LONGTEXT not null
);
            "#,
        )
        .execute(&pool)
        .await?;

        // create data column for old version <= 0.10.9
        DB_QUERY_NUMS
            .with_label_values(&["select", "information_schema.columns", ""])
            .inc();
        let has_data_column = sqlx::query_scalar::<_,i64>("SELECT count(*) FROM INFORMATION_SCHEMA.COLUMNS WHERE table_name='scheduled_jobs' AND column_name='data';")
            .fetch_one(&pool)
            .await?;
        if has_data_column == 0 {
            add_data_column().await?;
        }
        Ok(())
    }

    async fn create_table_index(&self) -> Result<()> {
        create_index(IndexStatement::new(
            "scheduled_jobs_key_idx",
            "scheduled_jobs",
            false,
            &["module_key"],
        ))
        .await?;
        create_index(IndexStatement::new(
            "scheduled_jobs_org_key_idx",
            "scheduled_jobs",
            false,
            &["org", "module_key"],
        ))
        .await?;
        create_index(IndexStatement::new(
            "scheduled_jobs_org_module_key_idx",
            "scheduled_jobs",
            true,
            &["org", "module", "module_key"],
        ))
        .await?;

        Ok(())
    }

    /// The count of jobs for the given module (Report/Alert etc.)
    async fn len_module(&self, module: TriggerModule) -> usize {
        let pool = CLIENT_RO.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let ret = match sqlx::query(
            r#"
SELECT CAST(COUNT(*) AS SIGNED) AS num FROM scheduled_jobs WHERE module = ?;"#,
        )
        .bind(module)
        .fetch_one(&pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("[MYSQL] triggers len error: {}", e);
                return 0;
            }
        };
        match ret.try_get::<i64, &str>("num") {
            Ok(v) => v as usize,
            _ => 0,
        }
    }

    /// Pushes a Trigger job into the queue
    async fn push(&self, trigger: Trigger) -> Result<()> {
        let pool = CLIENT.clone();
        let mut tx = pool.begin().await?;

        DB_QUERY_NUMS
            .with_label_values(&["insert", "scheduled_jobs", ""])
            .inc();
        if let Err(e) = sqlx::query(
            r#"
INSERT IGNORE INTO scheduled_jobs (org, module, module_key, is_realtime, is_silenced, status, retries, next_run_at, start_time, end_time, data)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ? , ?, ?);
        "#,
        )
        .bind(&trigger.org)
        .bind(&trigger.module)
        .bind(&trigger.module_key)
        .bind(trigger.is_realtime)
        .bind(trigger.is_silenced)
        .bind(&trigger.status)
        .bind(0)
        .bind(trigger.next_run_at)
        .bind(0)
        .bind(0)
        .bind(&trigger.data)
        .execute(&mut *tx)
        .await
        {
            if let Err(e) = tx.rollback().await {
                log::error!("[MYSQL] rollback push scheduled_jobs error: {}", e);
            }
            return Err(e.into());
        }

        if let Err(e) = tx.commit().await {
            log::error!("[MYSQL] commit push scheduled_jobs error: {}", e);
            return Err(e.into());
        }

        // For now, only send realtime alert triggers
        if trigger.module == TriggerModule::Alert && trigger.is_realtime {
            let key = format!(
                "{TRIGGERS_KEY}{}/{}/{}",
                trigger.module, &trigger.org, &trigger.module_key
            );
            let cluster_coordinator = db::get_coordinator().await;
            cluster_coordinator
                .put(&key, Bytes::from(""), true, None)
                .await?;
        }
        Ok(())
    }

    /// Deletes the Trigger job matching the given parameters
    async fn delete(&self, org: &str, module: TriggerModule, key: &str) -> Result<()> {
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["delete", "scheduled_jobs", key])
            .inc();
        sqlx::query(
            r#"DELETE FROM scheduled_jobs WHERE org = ? AND module = ? AND module_key = ?;"#,
        )
        .bind(org)
        .bind(&module)
        .bind(key)
        .execute(&pool)
        .await?;

        // For now, only send alert triggers
        if module == TriggerModule::Alert {
            // It will send event even if the alert is not realtime alert.
            // But that is okay, for non-realtime alerts, since the triggers are not
            // present in the cache at all, it will just do nothing.
            let key = format!("{TRIGGERS_KEY}{module}/{org}/{key}");
            let cluster_coordinator = db::get_coordinator().await;
            cluster_coordinator.delete(&key, false, true, None).await?;
        }
        Ok(())
    }

    /// Updates the status of the Trigger job
    async fn update_status(
        &self,
        org: &str,
        module: TriggerModule,
        key: &str,
        status: TriggerStatus,
        retries: i32,
        data: Option<&str>,
    ) -> Result<()> {
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["update", "scheduled_jobs", key])
            .inc();
        let query = match data {
            Some(data) => {
                sqlx::query(
                    r#"UPDATE scheduled_jobs SET status = ?, retries = ?, data = ? WHERE org = ? AND module_key = ? AND module = ?;"#,
                )
                .bind(status)
                .bind(retries)
                .bind(data)
                .bind(org)
                .bind(key)
                .bind(&module)
            },
            None => {
                sqlx::query(
                    r#"UPDATE scheduled_jobs SET status = ?, retries = ? WHERE org = ? AND module_key = ? AND module = ?;"#,
                )
                .bind(status)
                .bind(retries)
                .bind(org)
                .bind(key)
                .bind(&module)
            }
        };

        query.execute(&pool).await?;

        // For status update of triggers, we don't need to send put events
        // to cluster coordinator for now as it only changes the status and retries
        // fields of scheduled jobs and not anything else
        Ok(())
    }

    async fn update_trigger(&self, trigger: Trigger, clone: bool) -> Result<()> {
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["update", "scheduled_jobs", &trigger.module_key])
            .inc();

        let query = if clone {
            sqlx::query(
                r#"UPDATE scheduled_jobs
    SET status = ?, start_time = ?, end_time = ?, retries = ?, next_run_at = ?, is_realtime = ?, is_silenced = ?, data = ?
    WHERE org = ? AND module_key = ? AND module = ?;"#,
            )
            .bind(trigger.status)
            .bind(trigger.start_time)
            .bind(trigger.end_time)
            .bind(trigger.retries)
            .bind(trigger.next_run_at)
            .bind(trigger.is_realtime)
            .bind(trigger.is_silenced)
            .bind(&trigger.data)
            .bind(&trigger.org)
            .bind(&trigger.module_key)
            .bind(&trigger.module)
        } else {
            sqlx::query(
                r#"UPDATE scheduled_jobs
    SET status = ?, retries = ?, next_run_at = ?, is_realtime = ?, is_silenced = ?, data = ?
    WHERE org = ? AND module_key = ? AND module = ?;"#,
            )
            .bind(trigger.status)
            .bind(trigger.retries)
            .bind(trigger.next_run_at)
            .bind(trigger.is_realtime)
            .bind(trigger.is_silenced)
            .bind(&trigger.data)
            .bind(&trigger.org)
            .bind(&trigger.module_key)
            .bind(&trigger.module)
        };

        query.execute(&pool).await?;

        // For now, only send realtime alert triggers
        if trigger.module == TriggerModule::Alert && trigger.is_realtime {
            let key = format!(
                "{TRIGGERS_KEY}{}/{}/{}",
                trigger.module, &trigger.org, &trigger.module_key
            );
            let cluster_coordinator = db::get_coordinator().await;
            cluster_coordinator
                .put(&key, Bytes::from(""), true, None)
                .await?;
        }
        Ok(())
    }

    /// Keeps the trigger alive
    async fn keep_alive(&self, ids: &[i64], alert_timeout: i64, report_timeout: i64) -> Result<()> {
        let now = now_micros();
        let report_max_time = now
            + Duration::try_seconds(report_timeout)
                .unwrap()
                .num_microseconds()
                .unwrap();
        let alert_max_time = now
            + Duration::try_seconds(alert_timeout)
                .unwrap()
                .num_microseconds()
                .unwrap();

        let sql = format!(
            "UPDATE scheduled_jobs SET end_time = CASE WHEN module = ? THEN ? ELSE ? END WHERE id IN ({});",
            ids.iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let pool = CLIENT.clone();
        sqlx::query(&sql)
            .bind(TriggerModule::Alert)
            .bind(alert_max_time)
            .bind(report_max_time)
            .execute(&pool)
            .await?;

        Ok(())
    }

    /// Returns the Trigger jobs with "Waiting" status.
    /// Steps:
    /// - Read the records with status "Waiting", oldest createdAt, next_run_at <= now and limit =
    ///   `concurrency`
    /// - Skip locked rows and lock the read rows with "FOR UPDATE SKIP LOCKED"
    /// - Changes their statuses from "Waiting" to "Processing"
    /// - Commits as a single transaction
    /// - Returns the Trigger jobs
    async fn pull(
        &self,
        concurrency: i64,
        alert_timeout: i64,
        report_timeout: i64,
    ) -> Result<Vec<Trigger>> {
        log::debug!("Start pulling scheduled_job");
        let now = now_micros();
        let report_max_time = now
            + Duration::try_seconds(report_timeout)
                .unwrap()
                .num_microseconds()
                .unwrap();
        let alert_max_time = now
            + Duration::try_seconds(alert_timeout)
                .unwrap()
                .num_microseconds()
                .unwrap();

        let lock_pool = CLIENT.clone();
        let lock_key = "scheduler_pull_lock".to_string();
        let lock_id = config::utils::hash::gxhash::new().sum64(&lock_key);
        let lock_sql = format!(
            "SELECT GET_LOCK('{lock_id}', {})",
            config::get_config().limit.meta_transaction_lock_timeout
        );
        let unlock_sql = format!("SELECT RELEASE_LOCK('{lock_id}')");
        let mut lock_tx = lock_pool.begin().await?;
        DB_QUERY_NUMS
            .with_label_values(&["get_lock", "scheduled_jobs", ""])
            .inc();
        match sqlx::query_scalar::<_, i64>(&lock_sql)
            .fetch_one(&mut *lock_tx)
            .await
        {
            Ok(v) => {
                if v != 1 {
                    if let Err(e) = lock_tx.rollback().await {
                        log::error!("[SCHEDULER] rollback lock for pull scheduled_jobs error: {e}");
                    }
                    return Err(Error::from(DbError::DBOperError(
                        "LockTimeout".to_string(),
                        lock_key,
                    )));
                }
            }
            Err(e) => {
                if let Err(e) = lock_tx.rollback().await {
                    log::error!("[SCHEDULER] rollback lock for pull scheduled_jobs error: {e}");
                }
                return Err(e.into());
            }
        };

        let pool = CLIENT.clone();
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(e) => {
                if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
                    log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
                }
                if let Err(e) = lock_tx.commit().await {
                    log::error!("[SCHEDULER] commit for unlock pull scheduled_jobs error: {e}");
                }
                return Err(e.into());
            }
        };

        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let job_ids: Vec<TriggerId> = match sqlx::query_as::<_, TriggerId>(
            r#"SELECT id
FROM scheduled_jobs
WHERE status = ? AND next_run_at <= ? AND NOT (is_realtime = ? AND is_silenced = ?)
ORDER BY next_run_at
LIMIT ?;
            "#,
        )
        .bind(TriggerStatus::Waiting)
        .bind(now)
        .bind(true)
        .bind(false)
        .bind(concurrency)
        .fetch_all(&mut *tx)
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                if let Err(e) = tx.rollback().await {
                    log::error!("[SCHEDULER] rollback select jobs for update error: {}", e);
                }
                DB_QUERY_NUMS
                    .with_label_values(&["release_lock", "scheduled_jobs", ""])
                    .inc();
                if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
                    log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
                }
                if let Err(e) = lock_tx.commit().await {
                    log::error!("[SCHEDULER] commit for unlock pull scheduled_jobs error: {e}");
                }
                return Err(e.into());
            }
        };

        log::debug!(
            "scheduler pull: selected scheduled jobs for update: {}",
            job_ids.len()
        );
        if job_ids.is_empty() {
            if let Err(e) = tx.rollback().await {
                log::error!("[SCHEDULER] rollback scheduler pull error: {}", e);
            }
            DB_QUERY_NUMS
                .with_label_values(&["release_lock", "scheduled_jobs", ""])
                .inc();
            if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
                log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
            }
            if let Err(e) = lock_tx.commit().await {
                log::error!("[SCHEDULER] commit for unlock pull scheduled_jobs error: {e}");
            }
            return Ok(vec![]);
        }

        let job_ids: Vec<String> = job_ids.into_iter().map(|id| id.id.to_string()).collect();
        DB_QUERY_NUMS
            .with_label_values(&["update", "scheduled_jobs", ""])
            .inc();
        let query = format!(
            "UPDATE scheduled_jobs
SET status = ?, start_time = ?,
    end_time = CASE
        WHEN module = ? THEN ?
        ELSE ?
    END
WHERE id IN ({});",
            job_ids.join(",")
        );
        if let Err(e) = sqlx::query(&query)
            .bind(TriggerStatus::Processing)
            .bind(now)
            .bind(TriggerModule::Alert)
            .bind(alert_max_time)
            .bind(report_max_time)
            .execute(&mut *tx)
            .await
        {
            if let Err(e) = tx.rollback().await {
                log::error!("[MYSQL] rollback update scheduled jobs status error: {}", e);
            }
            DB_QUERY_NUMS
                .with_label_values(&["release_lock", "scheduled_jobs", ""])
                .inc();
            if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
                log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
            }
            if let Err(e) = lock_tx.commit().await {
                log::error!(
                    "[SCHEDULER] commit for unlock pull scheduled_jobs error: {}",
                    e
                );
            }
            return Err(e.into());
        }

        log::debug!("Update scheduled jobs for selected pull job ids");
        if let Err(e) = tx.commit().await {
            log::error!("[SCHEDULER] commit scheduler pull update error: {}", e);
            DB_QUERY_NUMS
                .with_label_values(&["release_lock", "scheduled_jobs", ""])
                .inc();
            if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
                log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
            }
            if let Err(e) = lock_tx.commit().await {
                log::error!(
                    "[SCHEDULER] commit for unlock pull scheduled_jobs error: {}",
                    e
                );
            }
            return Err(e.into());
        }

        DB_QUERY_NUMS
            .with_label_values(&["release_lock", "scheduled_jobs", ""])
            .inc();
        if let Err(e) = sqlx::query(&unlock_sql).execute(&mut *lock_tx).await {
            log::error!("[SCHEDULER] unlock pull scheduled_jobs error: {}", e);
        }
        if let Err(e) = lock_tx.commit().await {
            log::error!("[SCHEDULER] commit for unlock pull scheduled_jobs error: {e}");
        }

        let query = format!(
            "SELECT * FROM scheduled_jobs WHERE id IN ({});",
            job_ids.join(",")
        );
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let jobs: Vec<Trigger> = sqlx::query_as::<_, Trigger>(query.as_str())
            .fetch_all(&pool)
            .await?;
        log::debug!("Returning the pulled triggers: {}", jobs.len());
        Ok(jobs)
    }

    async fn get(&self, org: &str, module: TriggerModule, key: &str) -> Result<Trigger> {
        let pool = CLIENT_RO.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let query =
            r#"SELECT * FROM scheduled_jobs WHERE org = ? AND module = ? AND module_key = ?;"#;
        let job = match sqlx::query_as::<_, Trigger>(query)
            .bind(org)
            .bind(module.clone())
            .bind(key)
            .fetch_one(&pool)
            .await
        {
            Ok(job) => job,
            Err(_) => {
                return Err(Error::from(DbError::KeyNotExists(format!(
                    "{org}/{module}/{key}"
                ))));
            }
        };
        Ok(job)
    }

    async fn list(&self, module: Option<TriggerModule>) -> Result<Vec<Trigger>> {
        let pool = CLIENT_RO.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let jobs: Vec<Trigger> = if let Some(module) = module {
            let query = r#"SELECT * FROM scheduled_jobs WHERE module = ? ORDER BY id;"#;
            sqlx::query_as::<_, Trigger>(query)
                .bind(module)
                .fetch_all(&pool)
                .await?
        } else {
            let query = r#"SELECT * FROM scheduled_jobs ORDER BY id;"#;
            sqlx::query_as::<_, Trigger>(query).fetch_all(&pool).await?
        };
        Ok(jobs)
    }

    /// List all the jobs for the given module and organization
    async fn list_by_org(&self, org: &str, module: Option<TriggerModule>) -> Result<Vec<Trigger>> {
        let pool = CLIENT_RO.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let jobs: Vec<Trigger> = if let Some(module) = module {
            let query = r#"SELECT * FROM scheduled_jobs WHERE org = ? AND module = ? ORDER BY id;"#;
            sqlx::query_as::<_, Trigger>(query)
                .bind(org)
                .bind(module)
                .fetch_all(&pool)
                .await?
        } else {
            let query = r#"SELECT * FROM scheduled_jobs WHERE org = ? ORDER BY id;"#;
            sqlx::query_as::<_, Trigger>(query)
                .bind(org)
                .fetch_all(&pool)
                .await?
        };
        Ok(jobs)
    }

    /// Background job that frequently (30 secs interval) cleans "Completed" jobs or jobs with
    /// retries >= threshold set through environment
    async fn clean_complete(&self) -> Result<()> {
        let (include_max, mut max_retries) = get_scheduler_max_retries();
        if include_max {
            max_retries += 1;
        }
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["delete", "scheduled_jobs", ""])
            .inc();
        // Since alert scheduled_jobs contain last_satisfied_at field, we should not delete them
        sqlx::query(
            r#"DELETE FROM scheduled_jobs WHERE (status = ? OR retries >= ?) AND module != ?;"#,
        )
        .bind(TriggerStatus::Completed)
        .bind(max_retries)
        .bind(TriggerModule::Alert)
        .execute(&pool)
        .await?;
        Ok(())
    }

    /// Background job that watches for timeout of a job
    /// Steps:
    /// - Select all the records with status = "Processing"
    /// - calculate the current timestamp and difference from `start_time` of each record
    /// - Get the record ids with difference more than the given timeout
    /// - Update their status back to "Waiting" and increase their "retries" by 1
    async fn watch_timeout(&self) -> Result<()> {
        let pool = CLIENT.clone();
        let now = now_micros();
        DB_QUERY_NUMS
            .with_label_values(&["update", "scheduled_jobs", ""])
            .inc();
        let res = sqlx::query(
            r#"UPDATE scheduled_jobs
SET status = ?, retries = retries + 1
WHERE status = ? AND end_time <= ?
                "#,
        )
        .bind(TriggerStatus::Waiting)
        .bind(TriggerStatus::Processing)
        .bind(now)
        .execute(&pool)
        .await?;
        log::debug!(
            "[SCHEDULER] watch_timeout for scheduler updated {} rows",
            res.rows_affected()
        );
        Ok(())
    }

    async fn len(&self) -> usize {
        let pool = CLIENT_RO.clone();
        DB_QUERY_NUMS
            .with_label_values(&["select", "scheduled_jobs", ""])
            .inc();
        let ret = match sqlx::query(
            r#"
SELECT CAST(COUNT(*) AS SIGNED) AS num FROM scheduled_jobs;"#,
        )
        .fetch_one(&pool)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log::error!("[MYSQL] triggers len error: {}", e);
                return 0;
            }
        };
        match ret.try_get::<i64, &str>("num") {
            Ok(v) => v as usize,
            _ => 0,
        }
    }

    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    async fn clear(&self) -> Result<()> {
        let pool = CLIENT.clone();
        DB_QUERY_NUMS
            .with_label_values(&["delete", "scheduled_jobs", ""])
            .inc();
        match sqlx::query(r#"DELETE FROM scheduled_jobs;"#)
            .execute(&pool)
            .await
        {
            Ok(_) => log::info!("[SCHEDULER] scheduled_jobs table cleared"),
            Err(e) => log::error!("[MYSQL] error clearing scheduled_jobs table: {}", e),
        }

        Ok(())
    }
}

async fn add_data_column() -> Result<()> {
    log::info!("[MYSQL] Adding data column to scheduled_jobs table");
    let pool = CLIENT_DDL.clone();
    DB_QUERY_NUMS
        .with_label_values(&["alter", "scheduled_jobs", ""])
        .inc();
    if let Err(e) = sqlx::query(r#"ALTER TABLE scheduled_jobs ADD COLUMN data LONGTEXT NOT NULL;"#)
        .execute(&pool)
        .await
        && !e.to_string().contains("Duplicate column name")
    {
        // Check for the specific MySQL error code for duplicate column
        log::error!("[MYSQL] Unexpected error in adding column: {e}");
        return Err(e.into());
    }
    Ok(())
}
