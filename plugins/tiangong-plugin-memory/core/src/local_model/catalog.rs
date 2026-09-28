//! 内置本地模型清单。
//!
//! 每个档位固定一组 Embedding / Rerank 模型；文件按 HuggingFace 仓库的固定
//! revision 下载，并逐一校验大小与 sha256，保证不同机器得到完全相同的权重
//! （向量指纹一致，跨机器迁移记忆库无需重算）。
//!
//! 全部使用 int8 动态量化 ONNX（`onnx/model_quantized.onnx`），CPU 推理。

use crate::config::MemoryLocalTier;

/// 模型用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalModelKind {
    Embedding,
    Rerank,
}

impl LocalModelKind {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
        }
    }
}

/// 单个模型文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelFile {
    /// 仓库内路径。
    pub(crate) path: &'static str,
    pub(crate) size: u64,
    pub(crate) sha256: &'static str,
}

impl ModelFile {
    /// 本地保存的文件名（仓库路径的最后一段）。
    pub(crate) fn local_name(&self) -> &'static str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }
}

/// 一个本地模型的完整描述。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalModelSpec {
    /// 模型名（同时作为目录名与向量指纹中的模型名）。
    pub(crate) id: &'static str,
    pub(crate) kind: LocalModelKind,
    /// HuggingFace 仓库。
    pub(crate) repo: &'static str,
    /// 固定 revision（commit sha）。
    pub(crate) revision: &'static str,
    pub(crate) onnx: ModelFile,
    pub(crate) tokenizer: ModelFile,
    pub(crate) config: ModelFile,
    pub(crate) special_tokens_map: ModelFile,
    pub(crate) tokenizer_config: ModelFile,
    /// 向量维度（仅 Embedding）。
    pub(crate) dimension: usize,
}

impl LocalModelSpec {
    pub(crate) fn files(&self) -> [ModelFile; 5] {
        [
            self.tokenizer,
            self.config,
            self.special_tokens_map,
            self.tokenizer_config,
            self.onnx,
        ]
    }

    /// 全部文件总字节数。
    pub(crate) fn total_size(&self) -> u64 {
        self.files().iter().map(|file| file.size).sum()
    }
}

const fn file(path: &'static str, size: u64, sha256: &'static str) -> ModelFile {
    ModelFile { path, size, sha256 }
}

const QUANTIZED_ONNX: &str = "onnx/model_quantized.onnx";

/// 低档 Embedding：bge-small-zh-v1.5（512 维，约 24 MB）。
pub(crate) const BGE_SMALL_ZH: LocalModelSpec = LocalModelSpec {
    id: "bge-small-zh-v1.5",
    kind: LocalModelKind::Embedding,
    repo: "Xenova/bge-small-zh-v1.5",
    revision: "75c43b069aac4d136ba6bc1122f995fedcfd2781",
    onnx: file(
        QUANTIZED_ONNX,
        24_010_842,
        "15b717c382bcb518ba457b93ea6850ede7f4f1cd8937454aa06972366cd19bcc",
    ),
    tokenizer: file(
        "tokenizer.json",
        439_125,
        "48cea5d44424912a6fd1ea647bf4fe50b55ab8b1e5879c3275f80e339e8fae26",
    ),
    config: file(
        "config.json",
        716,
        "d4193ead3a810fd694fa8a31d7fc72fbaebc0668b603e398734bf2f6538ff42f",
    ),
    special_tokens_map: file(
        "special_tokens_map.json",
        125,
        "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    ),
    tokenizer_config: file(
        "tokenizer_config.json",
        367,
        "e6f3b96db926a37d4039995fbf5ad17de158dfb8f6343d607e4dbaad18d75f5a",
    ),
    dimension: 512,
};

/// 中档 Embedding：bge-base-zh-v1.5（768 维，约 103 MB）。
pub(crate) const BGE_BASE_ZH: LocalModelSpec = LocalModelSpec {
    id: "bge-base-zh-v1.5",
    kind: LocalModelKind::Embedding,
    repo: "Xenova/bge-base-zh-v1.5",
    revision: "71e50dc531959f9e04ebf190ea25b00261a0a186",
    onnx: file(
        QUANTIZED_ONNX,
        102_868_746,
        "b665f3bba56c3119bc76ba131ebcc544d720a7408cb11581bdf354aaa0198d43",
    ),
    tokenizer: file(
        "tokenizer.json",
        439_124,
        "7dfbf1966ebf99d471c3796e9b457329d2b2182b817e144f1e904b957745c839",
    ),
    config: file(
        "config.json",
        938,
        "855206771223efad2dfb8e212a716b20c4c71c8094309ca2da79d31bacb03276",
    ),
    special_tokens_map: file(
        "special_tokens_map.json",
        125,
        "b6d346be366a7d1d48332dbc9fdf3bf8960b5d879522b7799ddba59e76237ee3",
    ),
    tokenizer_config: file(
        "tokenizer_config.json",
        366,
        "9261e7d79b44c8195c1cada2b453e55b00aeb81e907a6664974b4d7776172ab3",
    ),
    dimension: 768,
};

/// 高档 Embedding：bge-m3（1024 维，多语言，约 570 MB）。
pub(crate) const BGE_M3: LocalModelSpec = LocalModelSpec {
    id: "bge-m3",
    kind: LocalModelKind::Embedding,
    repo: "Xenova/bge-m3",
    revision: "4de13258303883538bd53b696b452bf8099f0858",
    onnx: file(
        QUANTIZED_ONNX,
        569_694_530,
        "0826f8c1ab9edf1801db86c61919d4d108e8bfc0b809ec823ad366882ff0b77d",
    ),
    tokenizer: file(
        "tokenizer.json",
        17_082_821,
        "6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790",
    ),
    config: file(
        "config.json",
        770,
        "734a79bf12d388c1467a4e3ab625f45de7f6906cffcfb93a1eca1787504bed95",
    ),
    special_tokens_map: file(
        "special_tokens_map.json",
        964,
        "8c785abebea9ae3257b61681b4e6fd8365ceafde980c21970d001e834cf10835",
    ),
    tokenizer_config: file(
        "tokenizer_config.json",
        1_173,
        "7e4c1cc848840aeccdd763458c18dd525eb0f795c992e00ebe9c28554e7db2d4",
    ),
    dimension: 1024,
};

/// 低/中档 Rerank：bge-reranker-base（中英，约 280 MB）。
pub(crate) const BGE_RERANKER_BASE: LocalModelSpec = LocalModelSpec {
    id: "bge-reranker-base",
    kind: LocalModelKind::Rerank,
    repo: "Xenova/bge-reranker-base",
    revision: "280bcc27a84e0b898c251e06fddb25171bd9b101",
    onnx: file(
        QUANTIZED_ONNX,
        279_301_077,
        "dd98f3e67837d23210a6b7550c08cced4f61845b940ac45be3565840a10f3244",
    ),
    tokenizer: file(
        "tokenizer.json",
        17_098_079,
        "48564c5c7d3fa64d85d95e65414a542385f88b0f128fd8d4163fd7a57f2be05c",
    ),
    config: file(
        "config.json",
        782,
        "b6575b9d5be20d6747417c8e20c5a0db1636356e0b6d422d7244c628423c4d4c",
    ),
    special_tokens_map: file(
        "special_tokens_map.json",
        279,
        "d5469a60db23249c7f8945013d78df30b44b6bf686c6bb4740f4223f77b1b535",
    ),
    tokenizer_config: file(
        "tokenizer_config.json",
        443,
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
    dimension: 0,
};

/// 高档 Rerank：bge-reranker-v2-m3（多语言，约 590 MB）。
pub(crate) const BGE_RERANKER_V2_M3: LocalModelSpec = LocalModelSpec {
    id: "bge-reranker-v2-m3",
    kind: LocalModelKind::Rerank,
    repo: "onnx-community/bge-reranker-v2-m3-ONNX",
    revision: "6f5ff65298512715a1e669753bc754d2bc8f367b",
    onnx: file(
        QUANTIZED_ONNX,
        570_727_094,
        "912fc1215c2dbff6499700534bd8d31253af01573861abbfc43afd1fab6cce5d",
    ),
    tokenizer: file(
        "tokenizer.json",
        17_082_900,
        "8bf8afbfd11306bd872018c53bfdf2e160a56f8edbcf49933324404791c148d3",
    ),
    config: file(
        "config.json",
        848,
        "122e922dcfed6503c8721e6fe1daf090340c3d95ca7f3aa3a72730b321a51cfd",
    ),
    special_tokens_map: file(
        "special_tokens_map.json",
        964,
        "8c785abebea9ae3257b61681b4e6fd8365ceafde980c21970d001e834cf10835",
    ),
    tokenizer_config: file(
        "tokenizer_config.json",
        1_203,
        "b87c8703482b0300d3da30e201519aa641f6a450f5eb5bf1e624afbf70c74d80",
    ),
    dimension: 0,
};

/// 档位对应的 Embedding 模型。
pub(crate) fn embedding_model(tier: MemoryLocalTier) -> &'static LocalModelSpec {
    match tier {
        MemoryLocalTier::Low => &BGE_SMALL_ZH,
        MemoryLocalTier::Mid => &BGE_BASE_ZH,
        MemoryLocalTier::High => &BGE_M3,
    }
}

/// 档位对应的 Rerank 模型。
pub(crate) fn rerank_model(tier: MemoryLocalTier) -> &'static LocalModelSpec {
    match tier {
        MemoryLocalTier::Low | MemoryLocalTier::Mid => &BGE_RERANKER_BASE,
        MemoryLocalTier::High => &BGE_RERANKER_V2_M3,
    }
}

/// 档位 + 用途对应的模型。
pub(crate) fn model_for(kind: LocalModelKind, tier: MemoryLocalTier) -> &'static LocalModelSpec {
    match kind {
        LocalModelKind::Embedding => embedding_model(tier),
        LocalModelKind::Rerank => rerank_model(tier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [&LocalModelSpec; 5] = [
        &BGE_SMALL_ZH,
        &BGE_BASE_ZH,
        &BGE_M3,
        &BGE_RERANKER_BASE,
        &BGE_RERANKER_V2_M3,
    ];

    #[test]
    fn catalog_entries_are_well_formed() {
        for spec in ALL {
            assert_eq!(spec.revision.len(), 40, "{}", spec.id);
            for file in spec.files() {
                assert_eq!(file.sha256.len(), 64, "{} {}", spec.id, file.path);
                assert!(
                    file.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                    "{} {}",
                    spec.id,
                    file.path
                );
                assert!(file.size > 0);
            }
            let names = spec.files().map(|file| file.local_name());
            let unique = names.iter().collect::<std::collections::HashSet<_>>();
            assert_eq!(unique.len(), names.len(), "{} 本地文件名冲突", spec.id);
            match spec.kind {
                LocalModelKind::Embedding => assert!(spec.dimension > 0),
                LocalModelKind::Rerank => assert_eq!(spec.dimension, 0),
            }
        }
    }

    #[test]
    fn tiers_map_to_models() {
        assert_eq!(embedding_model(MemoryLocalTier::Low).dimension, 512);
        assert_eq!(embedding_model(MemoryLocalTier::Mid).dimension, 768);
        assert_eq!(embedding_model(MemoryLocalTier::High).dimension, 1024);
        assert_eq!(rerank_model(MemoryLocalTier::Mid).id, "bge-reranker-base");
        assert_eq!(rerank_model(MemoryLocalTier::High).id, "bge-reranker-v2-m3");
        // 低档总量应明显小于高档，档位才有意义。
        assert!(BGE_SMALL_ZH.total_size() * 10 < BGE_M3.total_size());
    }
}
