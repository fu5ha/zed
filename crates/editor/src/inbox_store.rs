#[cfg(not(any(test, feature = "test-support")))]
use crate::inbox_settings::InboxSettings;
use futures::channel::oneshot;
use gpui::{App, AppContext, Context, Entity, Global, Task};
use inbx::{
    models::{Revision, Source, ThreadWithMessages},
    storage::Store,
};
#[cfg(not(any(test, feature = "test-support")))]
use settings::Settings;
use std::{path::PathBuf, sync::mpsc};

pub enum Mutation {
    Create {
        file: String,
        content_id: String,
        body: String,
    },
    Reply {
        sqid: String,
        body: String,
    },
    Edit {
        sqid: String,
        message_id: u64,
        original_body: String,
        body: String,
    },
    Resolve {
        sqid: String,
        resolved: bool,
    },
}

type Snapshot = Vec<ThreadWithMessages>;
type StoreResult = Result<StoreUpdate, String>;

#[derive(Clone, Debug, Default)]
pub struct MutationResult {
    pub created_sqid: Option<String>,
    pub refresh_error: Option<String>,
}

struct SnapshotData {
    threads: Snapshot,
    serialized: String,
}

struct StoreUpdate {
    snapshot: Result<SnapshotData, String>,
    mutation: Option<MutationResult>,
}

struct Request {
    mutation: Option<Mutation>,
    response: oneshot::Sender<(u64, StoreResult)>,
}

struct GlobalInboxStore(Entity<InboxStore>);
impl Global for GlobalInboxStore {}

pub struct InboxStore {
    pub snapshot: Snapshot,
    pub error: Option<String>,
    pub show_resolved: bool,
    worker: Result<mpsc::Sender<Request>, String>,
    published_sequence: u64,
    snapshot_serialized: String,
    _poll_task: Task<()>,
}

impl InboxStore {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn test(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self {
            snapshot: Vec::new(),
            error: None,
            show_resolved: false,
            worker: Err("Test store does not accept mutations".into()),
            published_sequence: 0,
            snapshot_serialized: "[]".into(),
            _poll_task: Task::ready(()),
        })
    }

    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalInboxStore>() {
            return store.0.clone();
        }
        // Test dispatchers cannot drive a native SQLite worker; tests inject snapshots instead.
        #[cfg(any(test, feature = "test-support"))]
        let store = Self::test(cx);
        #[cfg(not(any(test, feature = "test-support")))]
        let store = cx.new(Self::new);
        cx.set_global(GlobalInboxStore(store.clone()));
        store
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn new(cx: &mut Context<Self>) -> Self {
        let explicit = std::env::var_os("PI_INBOX_DATABASE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                InboxSettings::get_global(cx)
                    .agent_dir
                    .as_ref()
                    .map(|directory| PathBuf::from(directory).join("data/pi-inbox/messages.sqlite"))
            });
        let database = if InboxSettings::get_global(cx)
            .agent_dir
            .as_ref()
            .is_some_and(|directory| directory.trim().is_empty())
        {
            Err("Pi Inbox agent_dir must not be empty.".into())
        } else {
            inbx::protocol::database_path(explicit.as_deref())
        };
        let worker = database.and_then(start_worker);
        let poll_worker = worker.clone();
        let poll_task = cx.spawn(async move |this, cx| {
            loop {
                match this.update(cx, |_, cx| InboxSettings::get_global(cx).enabled) {
                    Ok(true) => {
                        let result = request(&poll_worker, None).await;
                        if this
                            .update(cx, |this, cx| this.publish(result, false, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
            }
        });
        Self {
            snapshot: Vec::new(),
            error: worker.as_ref().err().cloned(),
            show_resolved: false,
            worker,
            published_sequence: 0,
            snapshot_serialized: "[]".into(),
            _poll_task: poll_task,
        }
    }

    pub fn mutate(
        &mut self,
        mutation: Mutation,
        cx: &mut Context<Self>,
    ) -> Task<Result<MutationResult, String>> {
        let worker = self.worker.clone();
        cx.spawn(async move |this, cx| {
            let response = request(&worker, Some(mutation)).await;
            let result = response
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|(_, result)| result.as_ref().map_err(Clone::clone))
                .and_then(|update| {
                    update
                        .mutation
                        .clone()
                        .ok_or_else(|| "Inbox worker returned no mutation outcome".to_string())
                });
            this.update(cx, |this, cx| this.publish(response, true, cx))
                .map_err(|error| error.to_string())?;
            result
        })
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        self.run_request(None, cx)
    }

    pub fn toggle_resolved(&mut self, cx: &mut Context<Self>) {
        self.show_resolved = !self.show_resolved;
        cx.notify();
    }

    fn run_request(
        &mut self,
        mutation: Option<Mutation>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let worker = self.worker.clone();
        cx.spawn(async move |this, cx| {
            let response = request(&worker, mutation).await;
            let result = response
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|(_, result)| result.as_ref().map_err(Clone::clone))
                .and_then(|update| update.snapshot.as_ref().map(|_| ()).map_err(Clone::clone));
            this.update(cx, |this, cx| this.publish(response, true, cx))
                .map_err(|error| error.to_string())?;
            result
        })
    }

    fn publish(
        &mut self,
        response: Result<(u64, StoreResult), String>,
        mutation: bool,
        cx: &mut Context<Self>,
    ) {
        let previous_error = self.error.clone();
        let mut snapshot_changed = false;
        match response {
            Ok((sequence, result)) => {
                if sequence < self.published_sequence {
                    return;
                }
                self.published_sequence = sequence;
                match result {
                    Ok(update) => match update.snapshot {
                        Ok(snapshot) => {
                            if self.snapshot_serialized != snapshot.serialized {
                                self.snapshot = snapshot.threads;
                                self.snapshot_serialized = snapshot.serialized;
                                snapshot_changed = true;
                            }
                            // A polling refresh must not erase a conflict the user needs to see.
                            if mutation {
                                self.error = None;
                            }
                        }
                        Err(error) => self.error = Some(error),
                    },
                    Err(error) => self.error = Some(error),
                }
            }
            Err(error) => self.error = Some(error),
        }
        if snapshot_changed || previous_error != self.error {
            cx.notify();
        }
    }
}

#[cfg_attr(feature = "test-support", allow(dead_code))]
fn start_worker(database: PathBuf) -> Result<mpsc::Sender<Request>, String> {
    let (sender, receiver) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("pi-inbox-store".into())
        .spawn(move || {
            // Keep initialization errors rather than reopening or resetting a bad database.
            let mut store = None;
            let mut sequence = 0;
            while let Ok(request) = receiver.recv() {
                sequence += 1;
                let result = match store.get_or_insert_with(|| Store::open(&database)) {
                    Ok(store) => apply_request(store, request.mutation),
                    Err(error) => Err(error.clone()),
                };
                // The receiver may disappear when its view closes; the write is never replayed.
                if request.response.send((sequence, result)).is_err() {
                    log::debug!("Inbox request receiver dropped");
                }
            }
        })
        .map_err(|error| format!("Could not start inbox worker: {error}"))?;
    Ok(sender)
}

async fn request(
    worker: &Result<mpsc::Sender<Request>, String>,
    mutation: Option<Mutation>,
) -> Result<(u64, StoreResult), String> {
    let worker = worker.as_ref().map_err(Clone::clone)?;
    let (response, receiver) = oneshot::channel();
    worker
        .send(Request { mutation, response })
        .map_err(|_| "Inbox worker stopped".to_string())?;
    receiver.await.map_err(|_| {
        "Inbox worker stopped before returning a result; refresh before retrying".into()
    })
}

fn apply_request(store: &mut Store, mutation: Option<Mutation>) -> StoreResult {
    let mutated = mutation.is_some();
    let mut outcome = MutationResult::default();
    match mutation {
        Some(Mutation::Create {
            file,
            content_id,
            body,
        }) => {
            outcome.created_sqid = Some(
                store
                    .create_thread(&file, &content_id, &body, Source::User, Revision::default())?
                    .thread
                    .sqid,
            );
        }
        Some(Mutation::Reply { sqid, body }) => {
            store.reply(&sqid, &body, Source::User)?;
        }
        Some(Mutation::Edit {
            sqid,
            message_id,
            original_body,
            body,
        }) => store.edit_message(&sqid, message_id, &original_body, &body)?,
        Some(Mutation::Resolve { sqid, resolved }) => {
            store.set_resolved(&sqid, resolved)?;
        }
        None => {}
    }
    finish_request(outcome, mutated, store.snapshot())
}

fn finish_request(
    mut outcome: MutationResult,
    mutated: bool,
    snapshot: Result<Snapshot, String>,
) -> StoreResult {
    let snapshot = snapshot
        .and_then(|threads| {
            let serialized = serde_json::to_string(&threads).map_err(|error| error.to_string())?;
            Ok(SnapshotData { threads, serialized })
        })
        .map_err(|error| {
            if mutated {
                format!("Inbox change was saved, but refreshing failed: {error}. Do not retry the change.")
            } else {
                error
            }
        });
    if mutated {
        outcome.refresh_error = snapshot.as_ref().err().cloned();
    }
    Ok(StoreUpdate {
        snapshot,
        mutation: mutated.then_some(outcome),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_mutation_survives_refresh_failure() -> anyhow::Result<()> {
        let update = finish_request(
            MutationResult {
                created_sqid: Some("created-thread".into()),
                refresh_error: None,
            },
            true,
            Err("Database is unavailable".into()),
        )
        .map_err(anyhow::Error::msg)?;
        let outcome = update
            .mutation
            .ok_or_else(|| anyhow::anyhow!("Missing mutation outcome"))?;
        assert_eq!(outcome.created_sqid.as_deref(), Some("created-thread"));
        assert!(
            outcome
                .refresh_error
                .as_deref()
                .is_some_and(|error| error.contains("change was saved"))
        );
        assert!(update.snapshot.is_err());
        Ok(())
    }

    fn send(
        worker: &Result<mpsc::Sender<Request>, String>,
        mutation: Option<Mutation>,
    ) -> Result<Snapshot, String> {
        futures::executor::block_on(request(worker, mutation))?
            .1?
            .snapshot
            .map(|snapshot| snapshot.threads)
    }

    #[test]
    fn worker_mutations_and_external_changes() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("messages.sqlite");
        let worker = start_worker(database.clone());
        let file = directory.path().join("source.rs");
        let content_id = inbx::anchors::create_content_id("first\nsecond\n", 2, 1, 1)
            .map_err(anyhow::Error::msg)?;
        let snapshot = send(
            &worker,
            Some(Mutation::Create {
                file: file.to_string_lossy().into_owned(),
                content_id,
                body: "Initial message".into(),
            }),
        )
        .map_err(anyhow::Error::msg)?;
        let thread = snapshot
            .first()
            .ok_or_else(|| anyhow::anyhow!("No thread"))?;
        let sqid = thread.thread.sqid.clone();
        let message_id = thread
            .messages
            .first()
            .ok_or_else(|| anyhow::anyhow!("No message"))?
            .id;
        send(
            &worker,
            Some(Mutation::Edit {
                sqid: sqid.clone(),
                message_id,
                original_body: "Initial message".into(),
                body: "Edited message".into(),
            }),
        )
        .map_err(anyhow::Error::msg)?;
        assert!(
            send(
                &worker,
                Some(Mutation::Edit {
                    sqid: sqid.clone(),
                    message_id,
                    original_body: "Initial message".into(),
                    body: "Stale edit".into(),
                }),
            )
            .is_err()
        );
        send(
            &worker,
            Some(Mutation::Reply {
                sqid: sqid.clone(),
                body: "User reply".into(),
            }),
        )
        .map_err(anyhow::Error::msg)?;
        let mut external = Store::open(&database).map_err(anyhow::Error::msg)?;
        external
            .reply(&sqid, "Agent reply", Source::Agent)
            .map_err(anyhow::Error::msg)?;
        let snapshot = send(&worker, None).map_err(anyhow::Error::msg)?;
        let thread = snapshot
            .first()
            .ok_or_else(|| anyhow::anyhow!("No thread"))?;
        assert_eq!(thread.messages.len(), 3);
        assert_eq!(
            thread.messages.first().map(|message| message.body.as_str()),
            Some("Edited message")
        );
        assert_eq!(
            thread.messages.last().map(|message| message.source),
            Some(Source::Agent)
        );
        let snapshot = send(
            &worker,
            Some(Mutation::Resolve {
                sqid: sqid.clone(),
                resolved: true,
            }),
        )
        .map_err(anyhow::Error::msg)?;
        assert!(
            snapshot
                .first()
                .is_some_and(|thread| thread.thread.resolved)
        );
        let snapshot = send(
            &worker,
            Some(Mutation::Resolve {
                sqid,
                resolved: false,
            }),
        )
        .map_err(anyhow::Error::msg)?;
        assert!(
            snapshot
                .first()
                .is_some_and(|thread| !thread.thread.resolved)
        );
        drop(external);
        drop(worker);
        Ok(())
    }

    #[test]
    fn invalid_database_is_not_reset() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("messages.sqlite");
        std::fs::write(&database, b"not a SQLite database")?;
        let worker = start_worker(database.clone());
        assert!(send(&worker, None).is_err());
        assert!(send(&worker, None).is_err());
        assert_eq!(std::fs::read(database)?, b"not a SQLite database");
        Ok(())
    }

    #[test]
    fn explicit_database_path_is_preserved() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("custom.sqlite");
        assert_eq!(
            inbx::protocol::database_path(Some(&database)).map_err(anyhow::Error::msg)?,
            database,
        );
        Ok(())
    }
}
