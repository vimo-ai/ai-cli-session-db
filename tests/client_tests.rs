//! Client 单元测试

#[cfg(feature = "client")]
mod tests {
    use ai_cli_session_db::client::{ClientConfig};
    use std::path::PathBuf;

    #[test]
    fn test_client_config_default() {
        let config = ClientConfig::default();

        // 验证默认值
        assert_eq!(config.component, "unknown");
        assert_eq!(config.connect_retries, 3);
        assert_eq!(config.retry_interval_ms, 500);

        // 验证数据目录包含 .vimo
        assert!(config.data_dir.to_str().unwrap().contains(".vimo"));
    }

    #[test]
    fn test_client_config_new() {
        let config = ClientConfig::new("test-component");

        assert_eq!(config.component, "test-component");
        // 其他使用默认值
        assert_eq!(config.connect_retries, 3);
        assert_eq!(config.retry_interval_ms, 500);
    }

    #[test]
    fn test_client_config_socket_path() {
        let mut config = ClientConfig::default();
        config.data_dir = PathBuf::from("/tmp/test-vimo");

        let socket_path = config.socket_path();
        assert_eq!(socket_path, PathBuf::from("/tmp/test-vimo/agent.sock"));
    }

    #[test]
    fn test_client_config_pid_path() {
        let mut config = ClientConfig::default();
        config.data_dir = PathBuf::from("/tmp/test-vimo");

        let pid_path = config.pid_path();
        assert_eq!(pid_path, PathBuf::from("/tmp/test-vimo/agent.pid"));
    }

    #[test]
    fn test_client_config_agent_binary_path() {
        let mut config = ClientConfig::default();
        config.data_dir = PathBuf::from("/tmp/test-vimo");

        let binary_path = config.default_agent_binary_path();
        assert_eq!(binary_path, PathBuf::from("/tmp/test-vimo/bin/vimo-agent"));
    }
}
