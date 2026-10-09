# xiaoai
xiaoai filter

## 注意
仅使用于已破解的小爱音箱，系统版本1.72可以使用，理论上其他版本也可以，只要开放了ssh，并且固件提供兼容的 `mipns-*` / `speech.usock` 语音前端。

## 使用方法
系统的可读写分区在 `/data`，启动后自动执行的脚本是 `/data/init.sh`。

将可执行程序 `xiaoai`、`config.yaml` 和 `start.sh` 拷贝到 `/data`：

```sh
chmod +x /data/xiaoai /data/start.sh
```

`init.sh` 内容如下：

```sh
#!/bin/ash

exec /data/start.sh start
```

必须通过 `start.sh` 启动。启动脚本会：

1. 启动原生 `mico_aivs_lab`，把它的 `speech.usock` 挪到 `speech.native.usock`
2. 让本程序绑定原来的 `speech.usock`，作为 `mipns-*` 和原生小爱之间的代理
3. 让 `mipns-*` 以 PCM 模式占用真实麦克风并检测唤醒词

手动启动、停止或恢复原生语音：

```sh
/data/start.sh start
/data/start.sh stop
/data/start.sh status
```

日志在 `/tmp/xiaoai.log`。调试时可设置 `RUST_LOG=debug`。

## config.yaml配置解析
| 配置项 | 说明 |
| --- | --- |
| port |                web服务监听的端口，web服务会提供/tts和/music/play两个api |
| detect_interval |     inotify 不可用时轮询原生 ASR 日志的间隔，单位 ms，默认 200 |
| speech_socket |      mipns 使用的 Unix socket，由本程序绑定 |
| native_speech_socket | 原生 mico_aivs_lab 的 speech socket，用于自带 ASR |
| instruction_log |    原生 ASR 结果日志，默认 `/tmp/mico_aivs_lab/instruction.log` |
| instructions |        指令集合 |
| match_type |          匹配类型，可选值有All 全匹配， Regex 正则表达式  默认为All，如为默认值则可不加此配置项 |
| content |            指令内容，如果是全匹配则要求语音识别内容和指令内容一致，正则表达式则为匹配有值则算通过 |
| command_type |        命令类型，可选值有Shell和Wol，Shell则表示会将command通过ash执行，Wol则是发送wol数据包，command里填写目标机器的MAC |
| command |             命令内容，支持content通过正则匹配到的内容作为command的参数 |
| result_exec_type|    结果执行方式，默认为TTS，文本转语音, 可选值为Music，即将result的内容作为播放音乐的url传入 |
| result |              如果是$开头则表示是通过json path从command的执行结果中提取指定的值，这就要求command的执行结果要是json |


config.yaml示例
```yaml
port: 8082
instructions:
  - content: 打开PC
    command_type: Wol
    command: 3A:7C:3F:D5:1E:8B
    result: 好的

  - content: 关闭PC
    command: curl --insecure -connect-timeout 2 -m 4 -s http://192.168.1.100:30000?pass=sdfscs
    result: 已关闭

  - match_type: Regex
    content: 播放(.*)的歌$
    command: curl --get --data-urlencode "keyword={p0}" –connect-timeout 2  http://192.168.2.102:3030/music # {p0}表示匹配到的第一个替换到这里,{p1} {p2}以此类推
    result: $.url # $开头表示用json path获取值，需要command返回结果为json
    result_exec_type: Music

  - content: 重启
    command: reboot
```

## 原理
1. 固件原生 `mipns-*` 继续占用真实麦克风，负责唤醒词检测。
2. 本程序代理 `speech.usock`：默认把音频转发给原生 `mico_aivs_lab`，使用音箱自带 ASR。
3. 识别结果如果在 `instructions` 列表中，立即把发给原生小爱的麦克风输入改成静音并取消本轮原生对话，然后执行自定义命令。
4. 识别结果不在列表中，保持转发，走原生小爱后续处理。

音频链路：

```
真实麦克风 -> mipns-* -> xiaoai 代理
                            ├─ 命中指令：静音后执行自定义命令
                            └─ 未命中：转发给 mico_aivs_lab 走原生流程
```

## web api
1. POST /tts {"text": ""}         

text的值就是小爱会说的话

2. POST /music/play {"url": ""}

url就是小爱要放的音乐的url

## 下载
GitHub Actions 会自动交叉编译 `aarch64-unknown-linux-musl` 并打包 `xiaoai`、`start.sh`、`config.yaml`：

- 推送到 `main` 后更新 [dev](https://github.com/LGinC/xiaoai/releases/tag/dev) 预发布
- 新建 GitHub Release 后把对应版本号的包上传到该 Release

## 交叉编译
首先需要安装docker或podman

然后添加target aarch64-unknown-linux-musl
`rustup target add  aarch64-unknown-linux-musl `

安装cross
`cargo install cross --git https://github.com/cross-rs/cross`

安装完毕后需要新开终端，最后执行编译命令
`cargo build --release --target  aarch64-unknown-linux-musl`

cross会自行拉取镜像然后在容器中运行编译，最后得到的可执行程序就在`./target/aarch64-unknown-linux-musl/release`下
