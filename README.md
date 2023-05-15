# xiaoai
 xiaoai filter

 ## 注意
 仅使用于已破解的小爱音箱，系统版本1.72可以使用，理论上其他版本也可以，只要开放了ssh

 ## 使用方法
 我系统的可读写分区是在/data, 启动后自动执行的脚本是/data/init.sh
 因此是将可执行程序xiaoai通过ssh拷贝到/data目录下，然后在init.sh中加入
 `/data/xiaoai > /dev/null 2>&1 &`

 完整的init.sh内容如下
 ```sh
#!/bin/ash

/data/xiaoai > /dev/null 2>&1 &
 ```

同时将config.yaml也拷贝和xiaoxi同一目录下，即/data/config.yaml

## config.yaml配置解析
| 配置项 | 说明 |
| --- | --- |
| port |                web服务监听的端口，web服务会提供/tts和/music/play两个api |
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
```

## 原理
1. 通过每隔1s读取`/tmp/mico_aivs_lab/instruction.log`文件，看文件里是否有语音识别结果
2. 如果有则先执行`ubus call mediaplayer player_play_operation  "{\"action\":\"pause\"}"`让小爱闭嘴
3. 把识别结果和配置文件中的指令进行匹配，匹配成功则执行对应的命令

## web api
1. POST /tts {"text": ""}         

text的值就是小爱会说的话

2. POST /music/play {"url": ""}

url就是小爱要放的音乐的url
