use std::{path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::model::{Event, Task, now};

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let connection = Connection::open(state_dir.join("team.sqlite3"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL,
                document TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id TEXT NOT NULL REFERENCES tasks(id),
                kind TEXT NOT NULL,
                at TEXT NOT NULL,
                data TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS events_by_task ON events(task_id, sequence);
             CREATE INDEX IF NOT EXISTS tasks_by_creation ON tasks(created_at, id);",
        )?;
        Ok(Self { connection })
    }

    pub fn get(&self, id: &str) -> Result<Task> {
        let document: Option<String> = self
            .connection
            .query_row("SELECT document FROM tasks WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        serde_json::from_str(&document.with_context(|| format!("unknown task {id}"))?)
            .context("corrupt stored task")
    }

    pub fn list_active(&self) -> Result<Vec<Task>> {
        let mut statement = self.connection.prepare(
            "SELECT document FROM tasks WHERE json_extract(document, '$.state') NOT IN ('archived','cancelled','failed') ORDER BY created_at, id"
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn task_page(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<(Vec<Value>, Option<String>)> {
        ensure!((1..=100).contains(&limit), "list limit must be 1..=100");
        let after_time: Option<String> = after
            .map(|id| {
                self.connection
                    .query_row("SELECT created_at FROM tasks WHERE id=?1", [id], |row| {
                        row.get(0)
                    })
                    .with_context(|| format!("unknown task cursor {id}"))
            })
            .transpose()?;
        let mut statement = self.connection.prepare(
            "SELECT json_object(
                'id',id,'title',json_extract(document,'$.spec.title'),
                'state',json_extract(document,'$.state'),'branch',json_extract(document,'$.branch'),
                'worker',json_extract(document,'$.worker'),'reviewer',json_extract(document,'$.reviewer'),
                'pr',json_extract(document,'$.pr'),'review_head',json_extract(document,'$.review_head'),
                'merge_sha',json_extract(document,'$.merge_sha'),'repaired_by',json_extract(document,'$.repaired_by'),
                'last_error_excerpt',substr(json_extract(document,'$.last_error'),1,500),
                'created_at',created_at,'updated_at',json_extract(document,'$.updated_at')
             ) FROM tasks WHERE (?1 IS NULL OR created_at>?1 OR (created_at=?1 AND id>?2))
             ORDER BY created_at,id LIMIT ?3"
        )?;
        let rows = statement
            .query_map(params![after_time, after, i64::from(limit) + 1], |row| {
                row.get::<_, String>(0)
            })?;
        let mut page = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        let mut last = None;
        for row in rows {
            let document = row?;
            if page.len() >= limit as usize
                || (!page.is_empty() && bytes + document.len() > 1024 * 1024)
            {
                more = true;
                break;
            }
            bytes += document.len();
            let summary: Value = serde_json::from_str(&document)?;
            last = Some(
                summary["id"]
                    .as_str()
                    .context("task summary has no identity")?
                    .to_owned(),
            );
            page.push(summary);
        }
        Ok((page, if more { last } else { None }))
    }

    pub fn event_page(
        &self,
        id: &str,
        after: i64,
        limit: u32,
    ) -> Result<(Vec<Event>, Option<i64>)> {
        ensure!(
            after >= 0 && limit <= 100,
            "events_after must be nonnegative and event_limit at most 100"
        );
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let mut statement = self.connection.prepare(
            "SELECT sequence,kind,at,data FROM events WHERE task_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3"
        )?;
        let rows = statement.query_map(params![id, after, i64::from(limit) + 1], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut page = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for row in rows {
            let (sequence, kind, at, data) = row?;
            if page.len() >= limit as usize
                || (!page.is_empty() && bytes + data.len() > 2 * 1024 * 1024)
            {
                more = true;
                break;
            }
            bytes += data.len();
            page.push(Event {
                sequence,
                task_id: id.to_owned(),
                kind,
                at,
                data: serde_json::from_str(&data)?,
            });
        }
        let next = if more {
            page.last().map(|event| event.sequence)
        } else {
            None
        };
        Ok((page, next))
    }

    pub fn find_request(&self, request_id: &str) -> Result<Option<Task>> {
        let document: Option<String> = self
            .connection
            .query_row(
                "SELECT document FROM tasks WHERE request_id = ?1",
                [request_id],
                |row| row.get(0),
            )
            .optional()?;
        document
            .map(|s| serde_json::from_str(&s).context("corrupt stored task"))
            .transpose()
    }

    pub fn insert(&self, task: &Task) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO tasks(id, request_id, created_at, document) VALUES (?1, ?2, ?3, ?4)",
            params![
                task.id,
                task.spec.request_id,
                task.created_at,
                serde_json::to_string(task)?
            ],
        )?;
        transaction.execute(
            "INSERT INTO events(task_id, kind, at, data) VALUES (?1, 'created', ?2, ?3)",
            params![task.id, task.created_at, serde_json::to_string(&task.spec)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save(&self, task: &mut Task, kind: &str, data: Value) -> Result<()> {
        task.updated_at = now();
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE tasks SET document = ?2 WHERE id = ?1",
            params![task.id, serde_json::to_string(task)?],
        )?;
        ensure!(changed == 1, "cannot update unknown task {}", task.id);
        transaction.execute(
            "INSERT INTO events(task_id, kind, at, data) VALUES (?1, ?2, ?3, ?4)",
            params![
                task.id,
                kind,
                task.updated_at,
                serde_json::to_string(&data)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn cancellation_pending(&self, id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM events requested
                WHERE requested.task_id = ?1 AND requested.kind = 'cancellation_requested'
                AND NOT EXISTS(
                    SELECT 1 FROM events finished
                    WHERE finished.task_id = requested.task_id AND finished.kind = 'cancelled'
                    AND finished.sequence > requested.sequence
                )
            )",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn events(&self, id: &str) -> Result<Vec<Event>> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, task_id, kind, at, data FROM events WHERE task_id = ?1 ORDER BY sequence"
        )?;
        let rows = statement.query_map([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (sequence, task_id, kind, at, data) = row?;
            Ok(Event {
                sequence,
                task_id,
                kind,
                at,
                data: serde_json::from_str(&data)?,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{StartTask, TaskState};

    fn task() -> Task {
        let time = now();
        Task {
            id: "task-1".into(),
            spec: StartTask {
                request_id: "same-request".into(),
                title: "Deliver behavior".into(),
                goal: "Observe behavior".into(),
                non_goals: vec![],
                acceptance: vec!["Actual result".into()],
                base_sha: "a".repeat(40),
                branch_slug: "behavior".into(),
                model: None,
                repairs_task_id: None,
                completes_original_goal: false,
            },
            state: TaskState::Starting,
            branch: "worker/task-1-behavior".into(),
            worktree: "/tmp/task-1".into(),
            worker: None,
            reviewer: None,
            pr: None,
            review_base: None,
            review_head: None,
            review_report: None,
            merge_authorized_head: None,
            merge_sha: None,
            post_ci: vec![],
            repaired_by: None,
            last_error: None,
            created_at: time.clone(),
            updated_at: time,
        }
    }

    #[test]
    fn request_identity_and_state_history_survive_reopen() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut task = task();
        {
            let store = Store::open(dir.path())?;
            store.insert(&task)?;
            task.state = TaskState::IntegrationBlocked;
            store.save(
                &mut task,
                "post_ci_failed",
                serde_json::json!({"sha": "exact-merge"}),
            )?;
            let mut duplicate = task.clone();
            duplicate.id = "task-2".into();
            assert!(store.insert(&duplicate).is_err());
        }
        let store = Store::open(dir.path())?;
        assert_eq!(
            store.find_request("same-request")?.unwrap().state,
            TaskState::IntegrationBlocked
        );
        let events = store.events("task-1")?;
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["created", "post_ci_failed"]
        );
        assert_eq!(events[1].data["sha"], "exact-merge");
        Ok(())
    }

    #[test]
    fn interrupted_cancellation_survives_errors_and_restart() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut task = task();
        {
            let store = Store::open(dir.path())?;
            store.insert(&task)?;
            task.state = TaskState::NeedsAttention;
            store.save(&mut task, "cancellation_requested", Value::Null)?;
            store.save(&mut task, "attention_required", json_error())?;
        }
        let store = Store::open(dir.path())?;
        assert!(store.cancellation_pending(&task.id)?);
        task.state = TaskState::Cancelled;
        store.save(&mut task, "cancelled", Value::Null)?;
        assert!(!store.cancellation_pending(&task.id)?);
        Ok(())
    }

    #[test]
    fn large_retained_history_is_retrievable_in_bounded_pages() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path())?;
        for index in 0..85 {
            let mut item = task();
            item.id = format!("history-{index:03}");
            item.spec.request_id = item.id.clone();
            item.spec.goal = "x".repeat(100_000);
            item.state = TaskState::Cancelled;
            store.insert(&item)?;
        }
        let mut after = None;
        let mut ids = std::collections::BTreeSet::new();
        loop {
            let (page, next) = store.task_page(after.as_deref(), 100)?;
            assert!(serde_json::to_vec(&page)?.len() <= 1024 * 1024);
            for item in page {
                assert!(ids.insert(item["id"].as_str().unwrap().to_owned()));
            }
            match next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        assert_eq!(ids.len(), 85);
        assert_eq!(store.get("history-084")?.spec.goal.len(), 100_000);
        assert!(store.list_active()?.is_empty());
        Ok(())
    }

    #[test]
    fn event_pages_preserve_large_entries_without_gaps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(directory.path())?;
        let mut item = task();
        store.insert(&item)?;
        for _ in 0..5 {
            store.save(
                &mut item,
                "large_feedback",
                serde_json::json!({"payload":"x".repeat(600_000)}),
            )?;
        }
        let mut after = 0;
        let mut sequences = std::collections::BTreeSet::new();
        let mut feedback = 0;
        loop {
            let (page, next) = store.event_page(&item.id, after, 100)?;
            assert!(serde_json::to_vec(&page)?.len() < 3 * 1024 * 1024);
            for event in page {
                assert!(sequences.insert(event.sequence));
                if event.kind == "large_feedback" {
                    assert_eq!(event.data["payload"].as_str().unwrap().len(), 600_000);
                    feedback += 1;
                }
            }
            match next {
                Some(cursor) => after = cursor,
                None => break,
            }
        }
        assert_eq!(feedback, 5);
        assert_eq!(sequences.len(), 6);
        Ok(())
    }

    fn json_error() -> Value {
        serde_json::json!({"error":"tmux unavailable during stop"})
    }
}
