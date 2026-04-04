pub mod app_server_backend;
pub mod exec_backend;
pub mod mapping;

pub use app_server_backend::CodexAppServerBackend;
pub use exec_backend::CodexExecBackend;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::backend::traits::AgentBackend;
    use crate::codex::CodexCli;
    use crate::config::CodexConfig;

    use super::CodexAppServerBackend;

    #[test]
    fn app_server_backend_reports_collaboration_mode_support() {
        let mut config = sample_config();
        config.use_app_server = true;
        let backend = CodexAppServerBackend::new(Arc::new(CodexCli::new(config)));
        assert!(backend.supports_collaboration_mode());
    }

    fn sample_config() -> CodexConfig {
        CodexConfig {
            launcher: vec!["codex".to_owned()],
            working_directory: ".".into(),
            use_app_server: false,
            app_server_url: "ws://127.0.0.1:54321".to_owned(),
            sandbox: None,
            ask_for_approval: None,
            search: false,
            http_proxy: None,
            https_proxy: None,
            all_proxy: None,
            no_proxy: None,
            additional_writable_dirs: Vec::new(),
            model: None,
            profile: None,
            full_auto: false,
            skip_git_repo_check: false,
            turn_timeout_secs: 90,
            extra_args: Vec::new(),
        }
    }
}
