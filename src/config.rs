use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// web服务监听端口
    pub port: u16,
    /// inotify 不可用时轮询 instruction.log 的间隔，单位 ms
    #[serde(default = "default_interval")]
    pub detect_interval: u64,
    /// mipns 连接的 speech.usock，由本程序绑定
    #[serde(default = "default_speech_socket")]
    pub speech_socket: String,
    /// 被挪走的原生 mico_aivs_lab speech socket，用于转发自带 ASR
    #[serde(default = "default_native_speech_socket")]
    pub native_speech_socket: String,
    /// 原生 ASR 结果日志
    #[serde(default = "default_instruction_log")]
    pub instruction_log: String,
    #[serde(default)]
    pub instructions: Vec<Instruction>,
}

fn default_interval() -> u64 {
    200
}

fn default_speech_socket() -> String {
    "/tmp/mico_aivs_lab/usock/speech.usock".to_string()
}

fn default_native_speech_socket() -> String {
    "/tmp/mico_aivs_lab/usock/speech.native.usock".to_string()
}

fn default_instruction_log() -> String {
    "/tmp/mico_aivs_lab/instruction.log".to_string()
}

impl Config {
    pub fn detect_interval_duration(&self) -> Duration {
        Duration::from_millis(self.detect_interval.max(50))
    }
}

#[derive(Debug, Deserialize)]
pub enum MatchType {
    /// 全匹配
    All,
    /// 正则表达式
    Regex,
}

impl Default for MatchType {
    fn default() -> Self {
        MatchType::All
    }
}

/// 命令类型
#[derive(Debug, Deserialize)]
pub enum CommandType {
    /// shell命令
    Shell,
    /// 发送wol数据包
    Wol,
}

impl Default for CommandType {
    fn default() -> Self {
        CommandType::Shell
    }
}

#[derive(Debug, Deserialize)]
pub enum ResultExecType {
    /// 文本转语音
    TTS,
    /// 播放音乐
    Music,
}

impl Default for ResultExecType {
    fn default() -> Self {
        ResultExecType::TTS
    }
}

#[derive(Debug, Deserialize)]
pub struct Instruction {
    #[serde(default)]
    pub match_type: MatchType,
    pub content: String,
    #[serde(default)]
    pub command_type: CommandType,
    pub command: String,
    /// 执行结果 如果为$开头则为json path匹配，需要命令执行的返回结果为json
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub result_exec_type: ResultExecType,
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn example_config_parses() {
        let config: Config = serde_yaml::from_str(include_str!("../config.yaml")).unwrap();
        assert_eq!(config.port, 8082);
        assert!(config.speech_socket.contains("speech.usock"));
        assert!(config.native_speech_socket.contains("speech.native.usock"));
        assert!(!config.instructions.is_empty());
    }
}
