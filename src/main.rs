use crate::config::Instruction;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(unix)]
use std::collections::VecDeque;
use std::env;
use std::process::Command;
use std::str::FromStr;
#[cfg(unix)]
use std::sync::Arc;
use tokio::task;
use tracing::info;
#[cfg(unix)]
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;
use warp::{hyper::StatusCode, Filter, Reply};
extern crate jsonpath_lib as jsonpath;
mod config;
mod native_asr;
mod recognize_result;
#[cfg(unix)]
mod speech;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let tts_route = warp::path("tts")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_tts);

    let music_route = warp::path("music")
        .and(warp::path("play"))
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_music);

    let routes = tts_route.or(music_route);

    let exe_path = env::current_exe().expect("Failed to get current executable path");
    let exe_dir = exe_path
        .parent()
        .expect("Failed to get parent directory of executable");
    let config_path = env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| exe_dir.join("config.yaml"));
    let config = serde_yaml::from_str::<config::Config>(
        &std::fs::read_to_string(&config_path)
            .expect(&format!("Failed to read config file at {:?}", &config_path)),
    )
    .expect("not correct format config");
    let mut regexes: HashMap<usize, Regex> = HashMap::new();
    for (i, ins) in config.instructions.iter().enumerate() {
        if let config::MatchType::Regex = ins.match_type {
            regexes.insert(i, Regex::new(&ins.content.as_str()).unwrap());
        }
    }

    info!("开始监听{}端口", config.port);
    let server = warp::serve(routes).run(([0, 0, 0, 0], config.port));
    task::spawn(server);

    #[cfg(unix)]
    {
        if let Err(err) = run_speech_loop(config, regexes).await {
            error!("speech loop stopped: {err:?}");
            std::process::exit(1);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (config, regexes);
        info!("speech.usock loop requires Linux; HTTP API is running");
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn run_speech_loop(
    config: config::Config,
    regexes: HashMap<usize, Regex>,
) -> anyhow::Result<()> {
    use crate::native_asr::spawn_instruction_log_watcher;
    use crate::speech::{SpeechProxy, SpeechProxyEvent};

    let config = Arc::new(config);
    let regexes = Arc::new(regexes);
    let mut proxy = SpeechProxy::bind(&config.speech_socket, &config.native_speech_socket).await?;
    let speech = proxy.handle();
    let mut asr_rx = spawn_instruction_log_watcher(
        config.instruction_log.clone().into(),
        config.detect_interval_duration(),
    );
    let mut handled: VecDeque<String> = VecDeque::with_capacity(8);

    info!(
        socket = %config.speech_socket,
        native = %config.native_speech_socket,
        log = %config.instruction_log,
        "waiting for mipns wake word; native ASR passthrough enabled"
    );

    loop {
        tokio::select! {
            event = proxy.recv() => {
                let Some(event) = event else {
                    anyhow::bail!("speech.usock proxy stopped unexpectedly");
                };
                match event {
                    SpeechProxyEvent::Registered { vendor, codec } => {
                        info!(
                            vendor,
                            codec = codec.as_deref().unwrap_or("pcm16"),
                            "mipns registered"
                        );
                    }
                    SpeechProxyEvent::Wake => {
                        info!("WAKE native mipns");
                    }
                    SpeechProxyEvent::Fatal(message) => anyhow::bail!(message),
                }
            }
            asr = asr_rx.recv() => {
                let Some(asr) = asr else {
                    anyhow::bail!("native ASR watcher stopped");
                };
                if handled.contains(&asr.id) {
                    continue;
                }
                let text = normalize_asr_text(&asr.text);
                let (instruction_index, params) =
                    not_include_instruction(&text, &config.instructions, &regexes);
                if instruction_index == -1 {
                    if asr.is_final {
                        info!(id = %asr.id, "NATIVE_PASSTHROUGH text={text}");
                    }
                    continue;
                }

                let instruction = config
                    .instructions
                    .get(instruction_index as usize)
                    .expect("matched instruction index is valid");
                if !should_intercept(&instruction.match_type, asr.is_final) {
                    continue;
                }

                info!(id = %asr.id, is_final = asr.is_final, "INTERCEPT text={text}");
                if let Err(err) = speech.mute_native().await {
                    warn!("failed to mute native microphone input: {err:?}");
                }
                handled.push_back(asr.id);
                if handled.len() > 8 {
                    handled.pop_front();
                }
                handle_command(params, instruction);
            }
        }
    }
}

fn normalize_asr_text(text: &str) -> String {
    text.trim()
        .trim_matches(|c: char| matches!(c, '。' | '！' | '？' | '.' | '!' | '?' | ',' | '，'))
        .trim()
        .to_string()
}

fn should_intercept(match_type: &config::MatchType, is_final: bool) -> bool {
    match match_type {
        config::MatchType::All => true,
        // 部分识别里捕获组还会变，宽模式会提前截走更具体的规则
        config::MatchType::Regex => is_final,
    }
}

fn handle_command(params: Vec<String>, ins: &config::Instruction) {
    let mut command = ins.command.clone();
    info!("command: {}", command);
    if params.len() > 0 {
        let mut pd = HashMap::new();
        for (i, p) in params.iter().enumerate() {
            pd.insert(format!("p{}", i), p.as_str());
        }
        match strfmt::strfmt(ins.command.as_str(), &pd) {
            Ok(s) => command = s,
            Err(_) => {
                eprintln!("cannot fmt command: {} params:{:?}", command, params);
                return;
            }
        }
    }

    info!("执行命令：{}", command);
    let output = match ins.command_type {
        config::CommandType::Shell => {
            let cmd = Command::new("ash")
                .arg("-c")
                .arg(format!("{}\n", &command))
                .output()
                .unwrap();
            match cmd.status.success() {
                true => String::from_utf8_lossy(&cmd.stdout).to_string(),
                false => {
                    println!("{}", String::from_utf8_lossy(&cmd.stderr).to_string());
                    if let Err(_) = Command::new("ash").arg("-c").arg("ubus call mibrain text_to_speech  \"{{\\\"text\\\":\\\"执行命令出错\\\"}}\"").status() {}
                    return;
                }
            }
        }
        config::CommandType::Wol => {
            match wol::send_wol(wol::MacAddr::from_str(&ins.command).unwrap(), None, None) {
                Ok(_) => String::from_str(r"成功").unwrap(),
                Err(_) => return,
            }
        }
    };

    info!("执行结果：{}", output.as_str());

    if ins.result.is_empty() {
        return;
    }

    let is_json_path = ins.result.starts_with("$");
    let result = match is_json_path {
        true => match jsonpath::select_as_str(output.as_str(), &ins.result) {
            Ok(s) => String::from_str(&s[1..s.len() - 1]).unwrap(),
            Err(_) => String::new(),
        },
        false => ins.result.clone(),
    };
    info!("result:{}", result);
    match ins.result_exec_type {
        config::ResultExecType::TTS => {
            if let Err(_) = Command::new("ash").arg("-c").arg(format!("ubus call mibrain text_to_speech \"{{\\\"text\\\":\\\"{}\\\",\\\"save\\\":0}}\"", result)).status(){};
        }
        config::ResultExecType::Music => {
            if let Err(_) = Command::new("ash").arg("-c").arg(format!("ubus call mediaplayer player_play_url \"{{\\\"url\\\":\\\"{}\\\",\\\"type\\\":1}}\"", result)).status(){};
        }
    }
}

pub fn not_include_instruction(
    _text: &str,
    ins_collection: &Vec<Instruction>,
    regs: &HashMap<usize, Regex>,
) -> (i32, Vec<String>) {
    if ins_collection.is_empty() || _text.is_empty() {
        return (-1, Vec::new());
    }

    for (i, ins) in ins_collection.iter().enumerate() {
        match ins.match_type {
            config::MatchType::All => {
                if _text == ins.content {
                    return (i as i32, Vec::new());
                }
            }
            config::MatchType::Regex => {
                let re = regs
                    .get(&i)
                    .expect(format!("cannot get instruction index is {}", i).as_str());
                if let Some(cap) = re.captures(_text) {
                    let params = (1..cap.len())
                        .map(|g| {
                            cap.get(g)
                                .map(|m| m.as_str().trim().to_string())
                                .unwrap_or_default()
                        })
                        .collect();
                    return (i as i32, params);
                }
            }
        }
    }
    return (-1, Vec::new());
}

async fn handle_tts(data: TextParam) -> Result<impl Reply, warp::Rejection> {
    let mut api_result = ApiResult { success: true };
    if let Err(_) = Command::new("ash")
        .arg("-c")
        .arg(format!(
            "ubus call mibrain text_to_speech \"{{\\\"text\\\":\\\"{}\\\",\\\"save\\\":0}}\"",
            data.text
        ))
        .status()
    {
        api_result.success = false;
    };
    Ok(api_result)
}

async fn handle_music(data: UrlParam) -> Result<impl Reply, warp::Rejection> {
    let mut api_result = ApiResult { success: true };
    if let Err(_) = Command::new("ash")
        .arg("-c")
        .arg(format!(
            "ubus call mediaplayer player_play_url \"{{\\\"url\\\":\\\"{}\\\",\\\"type\\\":1}}\"",
            data.url
        ))
        .status()
    {
        api_result.success = false;
    };
    Ok(api_result)
}

#[derive(Deserialize, Serialize)]
struct TextParam {
    text: String,
}

#[derive(Deserialize, Serialize)]
struct UrlParam {
    url: String,
}

#[derive(Deserialize, Serialize)]
struct ApiResult {
    success: bool,
}

impl Reply for ApiResult {
    fn into_response(self) -> warp::reply::Response {
        if self.success {
            warp::reply::with_status(warp::reply::json(&self), StatusCode::OK).into_response()
        } else {
            warp::reply::with_status(warp::reply::json(&self), StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Instruction, MatchType};

    #[test]
    fn matches_full_instruction() {
        let ins = vec![Instruction {
            match_type: MatchType::All,
            content: "打开PC".to_string(),
            command_type: Default::default(),
            command: "true".to_string(),
            result: String::new(),
            result_exec_type: Default::default(),
        }];
        let (index, params) = not_include_instruction("打开PC", &ins, &HashMap::new());
        assert_eq!(index, 0);
        assert!(params.is_empty());
    }

    #[test]
    fn skips_unmatched_instruction() {
        let ins = vec![Instruction {
            match_type: MatchType::All,
            content: "打开PC".to_string(),
            command_type: Default::default(),
            command: "true".to_string(),
            result: String::new(),
            result_exec_type: Default::default(),
        }];
        let (index, _) = not_include_instruction("今天天气", &ins, &HashMap::new());
        assert_eq!(index, -1);
    }

    #[test]
    fn strips_asr_punctuation() {
        assert_eq!(normalize_asr_text("打开PC。"), "打开PC");
        assert_eq!(normalize_asr_text("  关闭PC！ "), "关闭PC");
    }

    fn sample(match_type: MatchType, content: &str) -> Instruction {
        Instruction {
            match_type,
            content: content.to_string(),
            command_type: Default::default(),
            command: "true".to_string(),
            result: String::new(),
            result_exec_type: Default::default(),
        }
    }

    fn compiled(ins: &[Instruction]) -> HashMap<usize, Regex> {
        ins.iter()
            .enumerate()
            .filter(|(_, item)| matches!(item.match_type, MatchType::Regex))
            .map(|(i, item)| (i, Regex::new(&item.content).unwrap()))
            .collect()
    }

    #[test]
    fn regex_fills_capture_groups_in_order() {
        let ins = vec![sample(MatchType::Regex, r"从(.+)到(.+)$")];
        let regs = compiled(&ins);
        let (index, params) = not_include_instruction("从 客厅 到厨房", &ins, &regs);
        assert_eq!(index, 0);
        assert_eq!(params, vec!["客厅".to_string(), "厨房".to_string()]);
    }

    #[test]
    fn regex_without_capture_group_matches() {
        let ins = vec![sample(MatchType::Regex, r"^播放.+$")];
        let regs = compiled(&ins);
        let (index, params) = not_include_instruction("播放周杰伦", &ins, &regs);
        assert_eq!(index, 0);
        assert!(params.is_empty());
    }

    #[test]
    fn regex_optional_group_does_not_panic() {
        let ins = vec![sample(MatchType::Regex, r"^a(b)?c$")];
        let regs = compiled(&ins);
        let (index, params) = not_include_instruction("ac", &ins, &regs);
        assert_eq!(index, 0);
        assert_eq!(params, vec![String::new()]);
    }

    #[test]
    fn regex_intercept_waits_for_final_result() {
        assert!(should_intercept(&MatchType::All, false));
        assert!(should_intercept(&MatchType::All, true));
        assert!(!should_intercept(&MatchType::Regex, false));
        assert!(should_intercept(&MatchType::Regex, true));
    }

    #[test]
    fn example_config_music_regex() {
        let config: crate::config::Config =
            serde_yaml::from_str(include_str!("../config.yaml")).unwrap();
        let regs = compiled(&config.instructions);
        let hit = |text: &str| not_include_instruction(text, &config.instructions, &regs);

        let (index, params) = hit("播放周杰伦的歌");
        assert_eq!(index, 2);
        assert_eq!(params, vec!["周杰伦".to_string()]);

        let (index, params) = hit("播放周杰伦");
        assert_eq!(index, 3);
        assert_eq!(params, vec!["周杰伦".to_string()]);

        let (index, _) = hit("播放");
        assert_eq!(index, -1);

        let (index, _) = hit("我不想播放音乐");
        assert_eq!(index, -1);

        let (index, _) = hit("帮我播放周杰伦的歌");
        assert_eq!(index, -1);

        let (index, params) = hit(&normalize_asr_text("播放周杰伦的歌。"));
        assert_eq!(index, 2);
        assert_eq!(params, vec!["周杰伦".to_string()]);
    }
}
