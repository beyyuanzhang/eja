//! Reproducible full-journal retrieval evaluation with an explicit local ONNX encoder.
use eja_core::{Eja, OnnxEncoder, Source};
use rusqlite::Connection;
use std::{error::Error, path::PathBuf, time::Instant};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const PROBES: [(&str, &str); 6] = [
    (
        "我最能放松的时候，是星期六沿着江堤慢慢走，看水鸟落在芦苇上。",
        "周末想出去透口气，上回什么活动让我觉得自在？",
    ),
    (
        "花生会让我起疹子，点菜时得避开。",
        "聚餐点单，有哪样食材对我不安全？",
    ),
    (
        "门反锁时，备用钥匙放在楼下花盆底部。",
        "被锁在屋外进不了家，哪里有开门的东西？",
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
const DISTRACTORS: [&str; 8] = [
    "江堤附近周六举行自行车赛，需要临时封路。",
    "楼下花盆中的绿植每月施肥一次。",
    "今天的餐馆更换了点单系统，等待时间缩短。",
    "这周末大家决定在家看电影。",
    "本月地铁线路图调整了印刷颜色。",
    "冰箱冷藏室的照明已经修好。",
    "朋友给我推荐了一份新的会议议程。",
    "周五的仓库门换了闭门器。",
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
        .collect::<std::result::Result<_, _>>()?;
    let n = args.first().copied().unwrap_or(10_000);
    let variety = args.get(1).copied().unwrap_or(n);
    if n < 100 || variety == 0 {
        return Err("use at least 100 events and one background passage".into());
    }
    let encoder = load_encoder()?;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subject.db");
    let eja = Eja::open_with_encoder(&path, encoder.clone())?;
    let mut db = Connection::open(&path)?;
    let tx = db.transaction()?;
    let mut insert = tx.prepare(
        "INSERT INTO facts(body) VALUES(json_object('kind','noticed','source','person','text',?1))",
    )?;
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
    for id in 1..=n {
        let text = if let Some(i) = targets.iter().position(|v| *v == id) {
            PROBES[i].0.to_owned()
        } else if id <= DISTRACTORS.len() + 1 {
            DISTRACTORS[id - 2].to_owned()
        } else {
            let i = id % variety;
            format!(
                "{}第{}次{}。",
                domains[i % domains.len()],
                i / (events.len() * domains.len()),
                events[(i / domains.len()) % events.len()]
            )
        };
        insert.execute([text])?;
    }
    drop(insert);
    tx.commit()?;
    let start = Instant::now();
    eja.notice(Source::Person, PROBES[0].1)?;
    let view = eja.view()?;
    let related = view
        .related
        .as_ref()
        .ok_or("configure a semantic encoder first")?;
    assert!(
        view.concerns.is_empty()
            && !view
                .recent
                .iter()
                .any(|e| targets.contains(&(e.id as usize)))
    );
    let cold = start.elapsed();
    println!(
        "history={n} background_variety={variety} first_view_ms={:.1} automatic_first_ids={:?}",
        cold.as_secs_f64() * 1e3,
        related
            .iter()
            .map(|hit| hit.evidence.entry.id)
            .collect::<Vec<_>>()
    );
    drop(eja);
    let eja = Eja::open_with_encoder(&path, encoder)?;
    let mut top1 = 0;
    let mut top4 = 0;
    let mut times = Vec::new();
    // Exercise the normal view path each time. Later queries stay in the recent page.
    for ((_, cue), target) in PROBES.iter().zip(&targets) {
        eja.notice(Source::Person, *cue)?;
        let start = Instant::now();
        let hits = eja.view()?.related.unwrap();
        times.push(start.elapsed().as_secs_f64() * 1e3);
        let rank = hits
            .iter()
            .position(|hit| hit.evidence.entry.id == *target as i64)
            .map(|i| i + 1);
        top1 += usize::from(rank == Some(1));
        top4 += usize::from(rank.is_some());
        println!("target={target} rank={rank:?} {cue}");
    }
    let keys: i64 = db.query_row("SELECT count(*) FROM memory_keys", [], |r| r.get(0))?;
    times.sort_by(f64::total_cmp);
    println!(
        "keys={keys} top1={top1}/6 top4={top4}/6 warm_view_median_ms={:.2} database_MiB={:.1}",
        (times[2] + times[3]) / 2.,
        std::fs::metadata(&path)?.len() as f64 / 1048576.
    );
    Ok(())
}
