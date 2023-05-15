use crate::config::Instruction;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::metadata;
use std::fs::File;
use std::env;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;
use tokio::task;
use warp::{hyper::StatusCode, Filter, Reply};
extern crate jsonpath_lib as jsonpath;
mod config;
mod recognize_result;

#[tokio::main]
async fn main() {
    // 定义 POST /tts 路由
    let tts_route = warp::path("tts")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_tts);

    // 定义 POST /music/play 路由
    let music_route = warp::path("music")
        .and(warp::path("play"))
        .and(warp::post())
        .and(warp::body::json())
        .and_then(handle_music);

    // 将两个路由合并为一个
    let routes = tts_route.or(music_route);

    let file_path = std::path::Path::new("/tmp/mico_aivs_lab/instruction.log");
    // let file_path = "./test/instruction.log";
    let mut size: u64 = 0;
    let queue_max_length = 5;
    let mut queue: VecDeque<String> = VecDeque::with_capacity(queue_max_length);

    // 获取可执行文件的路径
    let exe_path = env::current_exe().expect("Failed to get current executable path");

    // 获取可执行文件所在的目录路径
    let exe_dir = exe_path
        .parent()
        .expect("Failed to get parent directory of executable");

    // 构建 config.yaml 文件的完整路径
    let config_path = exe_dir.join("config.yaml");
    //加载配置
    let config = serde_yaml::from_str::<config::Config>(
        &std::fs::read_to_string(&config_path)
            .expect(&format!("Failed to read config file at {:?}", &config_path))
    )
    .expect("not correct format config");
    let mut regexes: HashMap<usize, Regex> = HashMap::new();
    for (i, ins) in config.instructions.iter().enumerate() {
        if let config::MatchType::Regex = ins.match_type {
            regexes.insert(i, Regex::new(&ins.content.as_str()).unwrap());
        }
    }
    println!("开始监听{}端口", config.port);
    let server = warp::serve(routes).run(([0, 0, 0, 0], config.port));
    task::spawn(server);
    // let server_handle = task::spawn(server);
    println!("开始循环");
    loop {
        //日志文件不存在，稍等继续
        if !file_path.exists() {
            sleep(Duration::from_secs(1));
            continue;
        }
        let new_size = metadata(file_path).unwrap().len();
        if size != new_size {
            //判断文件大小是否有变化
            size = new_size;
            // println!("file changed");

            let reader = BufReader::new(File::open(file_path).unwrap());
            for line in reader.lines() {
                if let Ok(line) = line {
                    let recognize_result: recognize_result::RecognizeResult =
                        serde_json::from_str(&line).unwrap_or_default();
                    if recognize_result.payload.is_final
                        && recognize_result.header.namespace == "SpeechRecognizer"
                        && recognize_result.header.name == "RecognizeResult"
                    {
                        // println!("{:?}", recognize_result.payload);
                        //判断最近是否处理过
                        if queue.contains(&recognize_result.header.id) {
                            // println!("已处理请求 {}", recognize_result.header.id);
                            continue;
                        }
                        //判断识别结果是否在指令列表内
                        let (instruction_index, params) = not_include_instruction(
                            &recognize_result.payload.results.first().unwrap().text,
                            &config.instructions,
                            &regexes,
                        );
                        //未匹配则跳过
                        if instruction_index == -1 {
                            continue;
                        }

                        let mut loop_count = 0;
                        loop {
                            if loop_count > 50 {
                                break;
                            }

                            //获取是否为播放状态
                            let output = Command::new("ash")
                                .arg("-c")
                                .arg("ubus call mediaplayer player_get_play_status")
                                .output().expect("Failed to execute command ubus call mediaplayer player_play_operation");
                            let output_text = String::from_utf8_lossy(&output.stdout);
                            // println!("控制台输出: {}", output_text);
                            let parsed_obj: Value = serde_json::from_str(&output_text).unwrap();
                            let info_obj: Value = serde_json::from_str(
                                parsed_obj.get("info").unwrap().as_str().unwrap(),
                            )
                            .unwrap();
                            let play_status = info_obj.get("status").unwrap().as_i64().unwrap();
                            if play_status != 1 {
                                sleep(Duration::from_millis(300));
                                loop_count += 1;
                                continue;
                            }

                            if let Err(_) = Command::new("ash")
                                .arg("-c")
                                .arg("ubus call mediaplayer player_play_operation  {\\\"action\\\":\\\"pause\\\"}").status(){};
                            break;
                        }

                        //开始执行命令
                        handle_command(
                            params,
                            config.instructions.get(instruction_index as usize).unwrap(),
                        );

                        //处理完毕 将结果id加入队列避免重复处理
                        queue.push_back(recognize_result.header.id);
                        if queue.len() > queue_max_length {
                            queue.pop_front();
                        }
                        // println!("queue len {}", queue.len());
                    }
                }
            }
        }
        sleep(Duration::from_secs(1));
    }
    // server_handle.await.unwrap();
}

fn handle_command(params: Vec<String>, ins: &config::Instruction) {
    let mut command = ins.command.clone();
    println!("command: {}", command);
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

    println!("执行命令：{}", command);
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

    println!("执行结果：{}", output.as_str());

    if ins.result.is_empty() {
        return;
    }

    let is_json_path = ins.result.starts_with("$");
    let result = match is_json_path {
        true => match jsonpath::select_as_str(output.as_str(), &ins.result) {
            Ok(s) => s,
            Err(_) => String::new(),
        },
        false => ins.result.clone(),
    };
    let remove_brackets = &result[1..result.len()-1];
    println!("result:{}", remove_brackets);
    match ins.result_exec_type {
        config::ResultExecType::TTS => {
            if let Err(_) = Command::new("ash").arg("-c").arg(format!("ubus call mibrain text_to_speech \"{{\\\"text\\\":\\\"{}\\\",\\\"save\\\":0}}\"", remove_brackets)).status(){};
        }
        config::ResultExecType::Music => {
            if let Err(_) = Command::new("ash").arg("-c").arg(format!("ubus call mediaplayer player_play_url \"{{\\\"url\\\":\\\"{}\\\",\\\"type\\\":1}}\"", remove_brackets)).status(){};
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
                let mut matches = Vec::new();
                for cap in re.captures_iter(_text) {
                    matches.push(cap.get(1).unwrap().as_str().to_string());
                }
                if matches.len() > 0 {
                    return (i as i32, matches);
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
