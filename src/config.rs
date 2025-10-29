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

    /// The type of logger to use (e.g., "console", "file")
    #[arg(long, default_value_t = LoggerType::Vscode, env = "LOGGER")]
    pub logger: LoggerType,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoggerType {
    #[default]
    Vscode,
}

impl std::fmt::Display for LoggerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoggerType::Vscode => write!(f, "vscode"),
        }
    }
}

impl Config {
    /// Parse the configuration from command-line arguments and environment variables
    pub fn from_args() -> Self {
        Config::parse()
    }
}
