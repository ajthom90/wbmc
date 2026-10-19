// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use super::{FolderDelta, FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::{CoreError, Result};

/// Deterministic, offline provider. Each completed pass can be replayed to test
/// idempotence; an unfinished pass resumes at its persisted next-link offset.
pub struct FakeProvider {
    state: Mutex<FakeState>,
}

struct FakeState {
    folders: Vec<RemoteFolder>,
    messages: HashMap<String, Vec<RemoteMessage>>,
    deltas: HashMap<String, VecDeque<FolderDelta>>,
    fetch_errors: HashSet<(String, String)>,
    fail_after_pages: Option<u32>,
    reset_next: bool,
    auth_error: bool,
    auth_error_on_fetch: bool,
    pages: u32,
    calls: u32,
}

impl FakeProvider {
    pub fn new(folders: Vec<RemoteFolder>, messages: HashMap<String, Vec<RemoteMessage>>) -> Self {
        Self {
            state: Mutex::new(FakeState {
                folders,
                messages,
                deltas: HashMap::new(),
                fetch_errors: HashSet::new(),
                fail_after_pages: None,
                reset_next: false,
                auth_error: false,
                auth_error_on_fetch: false,
                pages: 0,
                calls: 0,
            }),
        }
    }

    pub fn provider_calls(&self) -> u32 {
        self.state.lock().expect("fake state poisoned").calls
    }

    /// Fail sync calls after this many successful pages, until explicitly cleared.
    /// Setting the knob starts a new page-counting window.
    pub fn set_fail_after_pages(&self, limit: Option<u32>) {
        let mut state = self.state.lock().expect("fake state poisoned");
        state.fail_after_pages = limit;
        state.pages = 0;
    }

    pub fn set_reset_next(&self, reset: bool) {
        self.state.lock().expect("fake state poisoned").reset_next = reset;
    }

    pub fn set_auth_error(&self, auth_error: bool) {
        self.state.lock().expect("fake state poisoned").auth_error = auth_error;
    }

    pub fn set_auth_error_on_fetch(&self, auth_error: bool) {
        self.state
            .lock()
            .expect("fake state poisoned")
            .auth_error_on_fetch = auth_error;
    }

    pub fn set_messages(&self, folder_remote_id: &str, messages: Vec<RemoteMessage>) {
        self.state
            .lock()
            .expect("fake state poisoned")
            .messages
            .insert(folder_remote_id.into(), messages);
    }

    /// Return a scripted delta on the next sync call for this folder.
    pub fn queue_delta(&self, folder_remote_id: &str, delta: FolderDelta) {
        self.state
            .lock()
            .expect("fake state poisoned")
            .deltas
            .entry(folder_remote_id.into())
            .or_default()
            .push_back(delta);
    }

    pub fn set_fetch_error(&self, folder_remote_id: &str, message_remote_id: &str, fail: bool) {
        let mut state = self.state.lock().expect("fake state poisoned");
        let key = (folder_remote_id.into(), message_remote_id.into());
        if fail {
            state.fetch_errors.insert(key);
        } else {
            state.fetch_errors.remove(&key);
        }
    }

    fn begin_call(&self) -> Result<MutexGuard<'_, FakeState>> {
        let mut state = self
            .state
            .lock()
            .map_err(|error| CoreError::Protocol(format!("fake state poisoned: {error}")))?;
        state.calls = state.calls.saturating_add(1);
        if state.auth_error {
            return Err(CoreError::NeedsSignIn {
                account_id: "fake".into(),
            });
        }
        Ok(state)
    }
}

#[async_trait]
impl MailProvider for FakeProvider {
    async fn list_folders(&self) -> Result<Vec<RemoteFolder>> {
        Ok(self.begin_call()?.folders.clone())
    }

    async fn sync_folder(
        &self,
        folder: &RemoteFolder,
        cursor: Option<FolderSyncState>,
        page_size: u32,
    ) -> Result<FolderDelta> {
        let mut state = self.begin_call()?;
        if state
            .fail_after_pages
            .is_some_and(|limit| state.pages >= limit)
        {
            return Err(CoreError::Network("injected page failure".into()));
        }
        if page_size == 0 {
            return Err(CoreError::InvalidInput("page_size must be positive".into()));
        }
        let reset = std::mem::take(&mut state.reset_next);
        let delta = match state
            .deltas
            .get_mut(&folder.remote_id)
            .and_then(VecDeque::pop_front)
        {
            Some(mut delta) => {
                delta.reset |= reset;
                delta
            }
            None => message_page(&state, &folder.remote_id, cursor, page_size, reset)?,
        };
        state.pages = state.pages.saturating_add(1);
        Ok(delta)
    }

    async fn fetch_raw(&self, folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>> {
        let state = self.begin_call()?;
        if state.auth_error_on_fetch {
            return Err(CoreError::NeedsSignIn {
                account_id: "fake".into(),
            });
        }
        if state
            .fetch_errors
            .contains(&(folder_remote_id.into(), message_remote_id.into()))
        {
            return Err(CoreError::Network("injected body failure".into()));
        }
        let message = state
            .messages
            .get(folder_remote_id)
            .and_then(|messages| {
                messages
                    .iter()
                    .find(|message| message.remote_id == message_remote_id)
            })
            .ok_or_else(|| CoreError::NotFound(format!("remote message {message_remote_id}")))?;
        Ok(format!(
            "From: {}\r\nSubject: {}\r\nContent-Type: text/plain\r\n\r\n{}",
            message.from.email, message.subject, message.snippet
        )
        .into_bytes())
    }
}

fn message_page(
    state: &FakeState,
    folder_remote_id: &str,
    cursor: Option<FolderSyncState>,
    page_size: u32,
    reset: bool,
) -> Result<FolderDelta> {
    let offset = if reset { 0 } else { page_offset(cursor)? };
    let mut messages = state
        .messages
        .get(folder_remote_id)
        .cloned()
        .unwrap_or_default();
    messages.sort_by(|a, b| {
        b.date_ms
            .cmp(&a.date_ms)
            .then_with(|| b.remote_id.cmp(&a.remote_id))
    });
    let end = offset
        .saturating_add(page_size as usize)
        .min(messages.len());
    let upserts = messages.get(offset..end).unwrap_or_default().to_vec();
    let more = end < messages.len();
    Ok(FolderDelta {
        upserts,
        deleted_remote_ids: vec![],
        flag_updates: vec![],
        server_remote_ids: None,
        new_state: FolderSyncState::Graph {
            delta_link: (!more).then(|| "fake:complete".into()),
            next_link: more.then(|| end.to_string()),
        },
        more,
        reset,
    })
}

fn page_offset(cursor: Option<FolderSyncState>) -> Result<usize> {
    match cursor {
        None
        | Some(FolderSyncState::Graph {
            next_link: None, ..
        }) => Ok(0),
        Some(FolderSyncState::Graph {
            next_link: Some(offset),
            ..
        }) => offset
            .parse()
            .map_err(|error| CoreError::Protocol(format!("invalid fake cursor: {error}"))),
        Some(FolderSyncState::Imap { .. }) => {
            Err(CoreError::Protocol("expected a fake Graph cursor".into()))
        }
    }
}
