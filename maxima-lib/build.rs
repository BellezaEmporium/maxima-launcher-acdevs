fn main() -> std::io::Result<()> {
    prost_build::compile_protos(
        &[
            "src/rtm/proto/rtm.proto",
            "src/rtm/proto/common.proto",
            "src/rtm/proto/presence.proto",
            "src/rtm/proto/chat.proto",
            "src/rtm/proto/config.proto",
            "src/rtm/proto/player.proto",
            "src/rtm/proto/antelope_common.proto",
        ],
        &["src/rtm/proto/"],
    )
}