use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tiangong_llm::{ProviderProtocol, complete_text_with_usage};
use tiangong_memory::{
    MemoryConfig, MemoryLlmSource, MemoryRemoteEndpoint, default_memory_config_path,
};

const SYSTEM_PROMPT: &str = "\
你是天工 Memory LLM smoke test。
只输出 JSON 对象，不要 Markdown，不要解释。
JSON 必须包含：
{
  \"marker\": \"memory_llm_smoke_ok\",
  \"summary\": \"...\"
}";

const DEFAULT_PROMPT: &str =
    "请确认你正在响应天工 Memory LLM smoke test，并用一句中文总结：专用 Memory LLM 配置可用。";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = SmokeArgs::parse()?;
    if args.help {
        print_usage();
        return Ok(());
    }
    if args.print_sample_config {
        print_sample_config()?;
        return Ok(());
    }

    let config = MemoryConfig::load_from_path(&args.config_path)
        .with_context(|| format!("读取 Memory 配置失败：{}", args.config_path.display()))?;
    let options = config.to_options();
    let Some(model) = options.model else {
        if args.allow_missing_config {
            eprintln!(
                "未找到完整 Memory LLM 配置，跳过真实调用：{}",
                args.config_path.display()
            );
            eprintln!("去掉 --allow-missing-config 后可强制要求真实模型路径可用。");
            return Ok(());
        }
        bail!(
            "未找到完整 Memory LLM 配置：{}\n可先运行 --print-sample-config 查看配置格式",
            args.config_path.display()
        );
    };

    println!("Memory LLM smoke test 开始");
    println!("配置文件：{}", args.config_path.display());
    println!("协议：{:?}", model.protocol);
    println!("模型：{}", model.model);
    println!("Base URL：{}", model.base_url);

    let started = Instant::now();
    let (response, usage) =
        complete_text_with_usage(&model, SYSTEM_PROMPT, &args.prompt, args.max_tokens)
            .await
            .context("Memory LLM 调用失败")?;
    let elapsed = started.elapsed();
    let value = parse_json_response(&response)?;
    let marker = value
        .get("marker")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("响应缺少 marker 字段：{response}"))?;
    if marker != "memory_llm_smoke_ok" {
        bail!("响应 marker 不符合预期：{marker}");
    }

    println!("校验：通过");
    if let Some(summary) = value.get("summary").and_then(Value::as_str) {
        println!("摘要：{summary}");
    }
    match usage {
        Some(usage) => println!(
            "Token：prompt={} completion={} total={}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        ),
        None => println!("Token：provider 未返回用量"),
    }
    println!("耗时：{} ms", elapsed.as_millis());
    Ok(())
}

#[derive(Debug)]
struct SmokeArgs {
    config_path: PathBuf,
    prompt: String,
    max_tokens: u32,
    allow_missing_config: bool,
    print_sample_config: bool,
    help: bool,
}

impl SmokeArgs {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut parsed = Self {
            config_path: default_memory_config_path(),
            prompt: DEFAULT_PROMPT.to_string(),
            max_tokens: 160,
            allow_missing_config: false,
            print_sample_config: false,
            help: false,
        };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    parsed.config_path = args
                        .next()
                        .map(PathBuf::from)
                        .ok_or_else(|| anyhow!("--config 需要路径参数"))?;
                }
                "--prompt" => {
                    parsed.prompt = args
                        .next()
                        .ok_or_else(|| anyhow!("--prompt 需要文本参数"))?;
                }
                "--max-tokens" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow!("--max-tokens 需要数字参数"))?;
                    parsed.max_tokens = value
                        .parse::<u32>()
                        .with_context(|| format!("解析 --max-tokens 失败：{value}"))?;
                }
                "--allow-missing-config" => parsed.allow_missing_config = true,
                "--print-sample-config" => parsed.print_sample_config = true,
                "-h" | "--help" => parsed.help = true,
                other => bail!("未知参数：{other}"),
            }
        }

        Ok(parsed)
    }
}

fn parse_json_response(response: &str) -> Result<Value> {
    let trimmed = response.trim();
    let json = if let Some(unfenced) = trimmed
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .or_else(|| {
            trimmed
                .strip_prefix("```")
                .and_then(|text| text.strip_suffix("```"))
        }) {
        unfenced.trim()
    } else if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        &trimmed[start..=end]
    } else {
        trimmed
    };
    serde_json::from_str(json)
        .with_context(|| format!("解析 Memory LLM 响应 JSON 失败：{response}"))
}

fn print_sample_config() -> Result<()> {
    let sample = MemoryConfig {
        model: Some(MemoryLlmSource::Remote(MemoryRemoteEndpoint {
            provider_key: None,
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "${MEMORY_LLM_API_KEY}".to_string(),
            model: "memory-model-name".to_string(),
            protocol: ProviderProtocol::OpenAiChatCompletions,
            timeout_ms: 60_000,
        })),
        ..Default::default()
    };
    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}

fn print_usage() {
    println!(
        "\
用法：
  cargo run -p tiangong-memory --example memory_llm_smoke -- [选项]

选项：
  --config <path>            指定 Memory 配置文件，默认 ~/.tiangong/memory/config.json
  --prompt <text>            指定 smoke test 提示词
  --max-tokens <n>           指定最大输出 token，默认 160
  --allow-missing-config     未配置 Memory LLM 时跳过真实调用并返回成功
  --print-sample-config      输出示例配置
  -h, --help                 显示帮助
"
    );
}
