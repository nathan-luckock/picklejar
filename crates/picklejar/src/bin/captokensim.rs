//! Capability tokens: a node verifies scoped, expiring grants offline, and
//! refuses forged, expired, and out-of-scope ones.
//!
//! ```text
//! cargo run --release --bin captokensim
//! ```

use std::process::ExitCode;

use picklejar::captoken::{issue, verify, Token};

fn check(key: &[u8], token: &Token, now: u64, mem: u64, caught: &mut u32, want_ok: bool) {
    match verify(key, token, now, mem) {
        Ok(()) => {
            if want_ok {
                *caught += 1;
            }
            println!("  access memory {mem} at t={now}  ->  GRANTED");
        }
        Err(e) => {
            if !want_ok {
                *caught += 1;
            }
            println!("  access memory {mem} at t={now}  ->  DENIED: {e}");
        }
    }
}

fn main() -> ExitCode {
    println!("\n=============== CAPABILITY TOKENS ===============");
    println!("a node verifies scoped, expiring grants with no callback\n");

    let key = b"authority shared secret";
    let token = issue(key, "acme", &[1, 2, 3], 100);
    println!("authority issues acme a token: scope memories [1,2,3], expires t=100\n");

    let mut good = 0;
    println!("the node checks requests, offline, against the shared secret:");
    check(key, &token, 50, 2, &mut good, true); // in scope, valid
    check(key, &token, 50, 9, &mut good, false); // out of scope
    check(key, &token, 150, 1, &mut good, false); // expired

    println!("\nan attacker widens the scope to memory 9 without the key:");
    let mut forged = token.clone();
    forged.scopes.push(9);
    check(key, &forged, 50, 9, &mut good, false); // bad signature

    println!("\n==================================================");
    if good == 4 {
        println!("VERDICT: the valid request was granted; out-of-scope, expired, and forged");
        println!("requests were all refused, with no call back to any central authority.");
    } else {
        println!("VERDICT: only {good}/4 outcomes correct; something is wrong.");
        return ExitCode::FAILURE;
    }
    println!("==================================================\n");
    ExitCode::SUCCESS
}
