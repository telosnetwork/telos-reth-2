use reth_primitives::TxType;
use reth_provider::providers::StaticFileProvider;
use reth_storage_api::TransactionsProvider;
use std::{env, process};

fn parse_u64(name: &str, value: Option<String>) -> u64 {
    let Some(value) = value else {
        eprintln!("missing {name}");
        process::exit(2);
    };
    value.parse().unwrap_or_else(|err| {
        eprintln!("invalid {name} {value:?}: {err}");
        process::exit(2);
    })
}

fn main() {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: telos-type2-helper STATIC_FILES START END_EXCLUSIVE [CHUNK_SIZE]");
        process::exit(2);
    });
    let start = parse_u64("start", args.next());
    let end = parse_u64("end", args.next());
    let chunk_size = args.next().map(|v| v.parse().unwrap()).unwrap_or(100_000u64);
    if end < start || chunk_size == 0 {
        eprintln!("invalid range or chunk size");
        process::exit(2);
    }

    let provider = StaticFileProvider::read_only(path, false)
        .unwrap_or_else(|err| {
            eprintln!("failed to open static files: {err:?}");
            process::exit(1);
        });

    let mut seen = 0u64;
    let mut typed = 0u64;
    let mut cursor = start;
    while cursor < end {
        let chunk_end = end.min(cursor.saturating_add(chunk_size));
        let transactions = provider
            .transactions_by_tx_range(cursor..chunk_end)
            .unwrap_or_else(|err| {
                eprintln!("failed transaction range {cursor}..{chunk_end}: {err:?}");
                process::exit(1);
            });
        for (offset, tx) in transactions.into_iter().enumerate() {
            seen += 1;
            if tx.tx_type() == TxType::Eip1559 {
                typed += 1;
                println!(
                    "type2 tx_number={} hash={:#x} transaction={:?}",
                    cursor + offset as u64,
                    tx.hash(),
                    tx
                );
            }
        }
        eprintln!("scanned through {chunk_end}; decoded={seen}; type2={typed}");
        cursor = chunk_end;
    }
    println!("summary range={start}..{end} decoded={seen} type2={typed}");
}
