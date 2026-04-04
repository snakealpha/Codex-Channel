use std::path::{Path, PathBuf};

use crate::domain::conversation::ConversationState;
use crate::domain::thread::ThreadRecord;

pub struct ThreadService {
    default_working_directory: PathBuf,
}

impl ThreadService {
    pub fn new(default_working_directory: PathBuf) -> Self {
        Self {
            default_working_directory,
        }
    }

    pub fn ensure_state_is_usable(&self, state: &mut ConversationState) {
        if state.threads.is_empty() {
            *state = ConversationState::with_default_thread(self.default_working_directory.clone());
            return;
        }

        if !state.threads.contains_key(&state.current_thread) {
            let fallback = state
                .threads
                .keys()
                .min()
                .cloned()
                .unwrap_or_else(|| "main".to_owned());
            state.current_thread = fallback;
        }
    }

    pub fn effective_working_directory(&self, thread: &ThreadRecord) -> PathBuf {
        effective_working_directory(thread, &self.default_working_directory)
    }

    pub fn next_thread_alias(&self, state: &ConversationState) -> String {
        next_thread_alias(state)
    }

    pub fn format_threads_message(&self, state: &ConversationState) -> String {
        format_threads_message(state, &self.default_working_directory)
    }
}

fn effective_working_directory(thread: &ThreadRecord, default_working_directory: &Path) -> PathBuf {
    thread
        .working_directory
        .clone()
        .unwrap_or_else(|| default_working_directory.to_path_buf())
}

fn next_thread_alias(state: &ConversationState) -> String {
    let mut candidate_index = 1;
    loop {
        let candidate = format!("thread-{candidate_index}");
        if !state.threads.contains_key(&candidate) {
            return candidate;
        }
        candidate_index += 1;
    }
}

fn format_threads_message(state: &ConversationState, default_working_directory: &Path) -> String {
    let mut lines = vec!["Available threads:".to_owned()];
    let mut aliases = state.threads.keys().cloned().collect::<Vec<_>>();
    aliases.sort();

    for alias in aliases {
        let thread = &state.threads[&alias];
        let marker = if alias == state.current_thread { "*" } else { "-" };
        let working_directory = effective_working_directory(thread, default_working_directory);
        let session = thread
            .codex_thread_id
            .as_deref()
            .map(short_session_id)
            .unwrap_or("new");
        lines.push(format!(
            "{marker} `{alias}`  cwd=`{}`  session=`{session}`  mode=`{}`",
            working_directory.display(),
            describe_thread_mode(thread)
        ));
    }

    lines.join("\n")
}

fn describe_thread_mode(thread: &ThreadRecord) -> &'static str {
    thread
        .collaboration_mode
        .as_ref()
        .map(crate::domain::mode::CollaborationModePreset::display_name)
        .unwrap_or("default")
}

fn short_session_id(session_id: &str) -> &str {
    let end = session_id.char_indices().nth(10).map_or(session_id.len(), |(index, _)| index);
    &session_id[..end]
}
