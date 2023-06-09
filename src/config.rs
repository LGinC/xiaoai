use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// web服务监听端口
    pub port: u16,
    /// 指令检测间隔 单位ms
    #[serde(default = "default_interval")]
    pub detect_interval: u64,

    #[serde(default)]
    pub instructions: Vec<Instruction>,
}

fn default_interval() -> u64 {
    1000
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
