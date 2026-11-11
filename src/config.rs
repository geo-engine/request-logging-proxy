use clap::{Parser, ValueEnum};
use url::Url;

/// Configuration for the Request Logging Proxy
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Config {
    /// The target URL to which requests will be proxied
    #[arg(long, env = "TARGET_URL")]
    pub target_url: Url,

    /// The port on which the proxy server will listen
    #[arg(long, default_value_t = 8123, env = "PORT")]
    pub port: u16,

    /// The type of logger to use (e.g., "vscode")
    #[arg(long, default_value_t = LoggerType::Vscode, env = "LOGGER")]
    pub logger: LoggerType,

    /// Overrides the authorization header and sends a Bearer authentication token (e.g., "TOKEN")
    #[arg(long, env = "OVERRIDE_BEARER_AUTH")]
    pub override_bearer_auth: Option<String>,

    /// Maximum number of bytes of a textual body to log; longer bodies are truncated
    #[arg(long, env = "MAX_LOGGED_BODY_BYTES")]
    pub max_logged_body_bytes: Option<usize>,

    /// Directory to store binary bodies in; the log references the files.
    /// Relative paths are written as given, so they resolve relative to the log file
    /// if the log is written to a file in the current working directory.
    #[arg(long, env = "BINARY_BODY_DIR")]
    pub binary_body_dir: Option<std::path::PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoggerType {
    #[default]
    Vscode,
    DevNull,
}

impl std::fmt::Display for LoggerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoggerType::Vscode => write!(f, "vscode"),
            LoggerType::DevNull => write!(f, "/dev/null"),
        }
    }
}

impl Config {
    /// Parse the configuration from command-line arguments and environment variables
    pub fn from_args() -> Self {
        Config::parse()
    }
}
