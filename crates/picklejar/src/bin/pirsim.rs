//! Private information retrieval: fetch a memory the server can't identify.
//!
//! ```text
//! cargo run --release --bin pirsim
//! ```

use std::process::ExitCode;

use picklejar::pir::{make_queries, reconstruct, Pir};

fn bits(q: &[bool]) -> String {
    q.iter()
        .take(24)
        .map(|b| if *b { '1' } else { '0' })
        .collect()
}

fn main() -> ExitCode {
    println!("\n=============== PRIVATE INFORMATION RETRIEVAL ===============");
    println!("fetch a memory without telling the server which one\n");

    // A database of named memories, replicated to two non-colluding servers.
    let names = [
        "ada: salary band",
        "bob: medical note",
        "cy: location history",
        "dee: API keys",
        "eve: chat logs",
        "fae: legal hold",
        "gus: biometrics",
        "hal: search history",
    ];
    let pir = Pir::new(
        names
            .iter()
            .map(|n| format!("{n:<24}").into_bytes())
            .collect(),
    );

    // The client secretly wants record 3 ("dee: API keys").
    let wanted = 3usize;
    let (qa, qb) = make_queries(pir.len(), wanted, 0x9E37_79B9);

    println!(
        "client secretly wants record {wanted} (\"{}\").\n",
        names[wanted].trim()
    );
    println!("server A sees a random selection: {}...", bits(&qa));
    println!("server B sees a random selection: {}...", bits(&qb));
    println!("(the two differ in exactly one bit, which neither server can locate)\n");

    let ans_a = pir.answer(&qa);
    let ans_b = pir.answer(&qb);
    let got = reconstruct(&ans_a, &ans_b);
    let recovered = String::from_utf8_lossy(&got).trim().to_string();

    println!("client xors the two answers -> recovered: \"{recovered}\"");

    // Confirm server A's view is the same no matter which record was wanted.
    let (qa_other, _) = make_queries(pir.len(), 6, 0x9E37_79B9);
    let a_independent = qa == qa_other;

    println!("\n==================================================");
    if recovered == names[wanted] && a_independent {
        println!(
            "VERDICT: the client recovered \"{}\" exactly, while server A's",
            names[wanted].trim()
        );
        println!("selection was identical whether it wanted record 3 or record 6, so neither");
        println!("non-colluding server learned which memory was retrieved.");
    } else {
        println!("VERDICT: unexpected (recovered '{recovered}', a_independent {a_independent}).");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
