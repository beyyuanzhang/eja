//! Benchmark derived semantic recall over a synthetic subject journal.
use eja_core::{Eja, Source};
use eja_memory::{MemoryIndex, OnnxEncoder};
use std::{error::Error, path::PathBuf, time::Instant};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const PROBES: [(&str, &str); 6] = [
    (
        "我平时喝茶不放糖，偏好淡一点的乌龙茶。",
        "给他准备热饮时要注意什么口味偏好？",
    ),
    (
        "家里门禁卡和备用钥匙都装在玄关抽屉的棕色信封里。",
        "我忘带进门的东西了，放哪儿了？",
    ),
    (
        "楼下药店周三和周日凌晨两点才关门，平时十点就打烊。",
        "半夜急需买退烧药，附近哪里还开着？",
    ),
    (
        "小满难过的时候，只想有人安静地听，不喜欢别人马上给建议。",
        "朋友情绪低落时希望我怎样陪伴？",
    ),
    (
        "我的夜班通勤靠末班地铁，错过就只能打车。",
        "加完班太晚回家，之前我是怎么解决交通问题的？",
    ),
    (
        "停电后冷藏的胰岛素放到了邻居的冰箱里。",
        "上次家里断电，需要低温保存的药转移到哪儿了？",
    ),
];

fn load_encoder() -> AppResult<OnnxEncoder> {
    let dir = std::env::var_os("EJA_MEMORY_MODEL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(".models")
                .join("multilingual-e5-small")
        });
    Ok(OnnxEncoder::load(dir)?)
}

fn main() -> AppResult<()> {
    let args: Vec<_> = std::env::args()
        .skip(1)
        .map(|s| s.parse::<usize>())
        .collect::<Result<_, _>>()?;
    let n = args.first().copied().unwrap_or(2_000);
    let variety = args.get(1).copied().unwrap_or(n);
    if n < 100 || variety == 0 {
        return Err("use at least 100 events and one background passage".into());
    }
    let encoder = load_encoder()?;
    let dir = tempfile::tempdir()?;
    let eja = Eja::open(dir.path().join("subject.db"))?;
    let mem_path = dir.path().join("memory.db");
    let targets: Vec<usize> = (0..PROBES.len()).map(|i| 1 + i * (n / 8)).collect();
    let domains = [
        "仓库",
        "教室",
        "剧场",
        "展馆",
        "茶室",
        "维修站",
        "图书馆",
        "车间",
    ];
    let events = [
        "核对收据",
        "盘点耗材",
        "调整座椅",
        "签收快件",
        "更新排期",
        "更换灯具",
        "清点桌牌",
        "清洗滤网",
        "修复设备",
        "登记交班",
    ];

    eja.transaction(|tx| {
        for id in 1..=n {
            let text = if let Some(i) = targets.iter().position(|v| *v == id) {
                PROBES[i].0.to_owned()
            } else {
                let i = id % variety;
                format!(
                    "{}第{}次{}。",
                    domains[i % domains.len()],
                    i / (events.len() * domains.len()),
                    events[(i / domains.len()) % events.len()]
                )
            };
            tx.notice(Source::Person, text)?;
        }
        Ok(())
    })?;

    let mut memory = MemoryIndex::open(&mem_path, encoder.clone())?;
    let start = Instant::now();
    let related = memory.search(&eja, PROBES[0].1)?;
    println!(
        "history={n} background_variety={variety} first_search_ms={:.1} first_ids={:?}",
        start.elapsed().as_secs_f64() * 1e3,
        related
            .iter()
            .map(|hit| hit.evidence.entry.id)
            .collect::<Vec<_>>()
    );
    drop(memory);

    let mut memory = MemoryIndex::open(&mem_path, encoder)?;
    for ((_, cue), target) in PROBES.iter().zip(&targets) {
        let hits = memory.search(&eja, cue)?;
        let rank = hits
            .iter()
            .position(|hit| hit.evidence.entry.id == *target as i64)
            .map(|idx| idx + 1);
        println!("target={target} rank={rank:?} {cue}");
    }
    Ok(())
}
