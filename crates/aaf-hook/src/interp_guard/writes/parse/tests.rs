use super::*;

/// O11-8 / E10: the script is scanned for assignments once, not once
/// per call or per name.
#[test]
fn assigned_runs_once_per_distinct_qualifier() {
    let mut code = String::from("class A:\n  def run(self):\n");
    for n in 0..1000 {
        code.push_str(&format!("    self.x({n})\n    other.y({n})\n"));
    }
    ASSIGNED_RUNS.with(|runs| runs.set(0));
    let script = Script::new(&code);
    let found = calls(&script);
    assert_eq!(found.len(), 2001);
    assert_eq!(ASSIGNED_RUNS.with(std::cell::Cell::get), 1);
}

/// The one-pass index finds what the per-name search found.
#[test]
fn assignments_match_the_per_name_rules() {
    let code = "a = 'x'\n$f = '>y'; @l = (1)\nb.c = 2\nab = 3\nxa = 4\na == 5\nk => 6\nn\n= 7\né = 8\n$$g = 9";
    let found = assignments(code);
    assert_eq!(found["a"], [" 'x'"]);
    assert_eq!(found["$f"], [" '>y'"]);
    assert_eq!(found["@l"], [" (1)"]);
    assert_eq!(found["ab"], [" 3"]);
    assert_eq!(found["n"], [" 7"]);
    assert_eq!(found["é"], [" 8"]);
    for absent in ["b", "c", "f", "l", "k", "$g", "g"] {
        assert!(!found.contains_key(absent), "{absent}");
    }
}

/// O11-13: a name assigned from itself (`i = i + 1`) is resolved once
/// per depth, not once per way of reaching it.
#[test]
fn self_assigned_counter_resolves_in_few_runs() {
    let mut code = String::from("i = 0\n");
    for _ in 0..30 {
        code.push_str("i = i + 1\nopen(f'out/{i}.txt', 'w')\n");
    }
    let script = Script::new(&code);
    let found = calls(&script);
    RESOLVE_RUNS.with(|runs| runs.set(0));
    for call in &found {
        for arg in &call.args {
            script.resolve(arg, 0);
        }
    }
    let runs = RESOLVE_RUNS.with(std::cell::Cell::get);
    assert!(runs < 200, "{runs} runs");
}

/// O11-13: what a reused script resolves is what a fresh one does.
#[test]
fn reused_script_resolves_like_a_fresh_one() {
    let code = "i = 0\ni = i + 1\np = sys.argv[1]\np = os.path.join(p, 'a')\nd = '.git'\nq = f'{d}/config'\nfor x in ['a', 'b']:\n  pass\nr = p + '/x'\n";
    let exprs = [
        "i",
        "f'out/{i}.txt'",
        "p",
        "q",
        "os.path.join(d, x)",
        "r",
        "Path(p) / 'y'",
        "d",
    ];
    let reused = Script::new(code);
    for _ in 0..2 {
        for expr in exprs {
            let again = reused.resolve(expr, 0);
            let fresh = Script::new(code).resolve(expr, 0);
            assert_eq!((again.lits, again.sure), (fresh.lits, fresh.sure), "{expr}");
        }
    }
}

/// R6-H6-6: `p = p + 'a'` many times over stands for a bounded number
/// of literals, and is not sure.
#[test]
fn self_concatenation_is_capped() {
    let mut code = String::from("p = ''\n");
    for _ in 0..50 {
        code.push_str("p = p + 'a'\n");
    }
    let found = Script::new(&code).resolve("p", 0);
    assert!(found.lits.len() <= MAX_LITS, "{}", found.lits.len());
    assert!(!found.sure);
}

#[test]
fn variable_receiver_and_interpolation_resolve() {
    let code = "p = Path('.git/hooks/x')\np.write_text('y')\nd='.git'\nq=f'{d}/config'";
    let script = Script::new(code);
    let call = calls(&script)
        .into_iter()
        .find(|call| call.name == "write_text")
        .unwrap();
    assert_eq!(call.receiver.as_deref(), Some(" Path('.git/hooks/x')"));
    let q = script.resolve("q", 0);
    assert!(q.sure);
    assert_eq!(q.lits, vec![".git/config".to_string()]);
    let unknown = script.resolve("Path(p2)", 0);
    assert!(!unknown.sure);
}

/// O13-R8-2: a join or an interpolation past [`MAX_LIT_BYTES`] is not
/// known, and no literal it leaves grows past the bound.
#[test]
fn literals_past_the_byte_bound_are_not_sure() {
    let code = format!("d = '{}'\nq = f'{{d}}{{d}}'\n", "a".repeat(3000));
    let script = Script::new(&code);
    let joined = script.resolve("os.path.join(d, d)", 0);
    assert!(!joined.sure);
    assert!(!joined.lits.is_empty());
    for lit in &joined.lits {
        assert!(lit.len() <= MAX_LIT_BYTES, "join: {}", lit.len());
    }
    let q = script.resolve("q", 0);
    assert!(!q.sure);
    assert!(!q.lits.is_empty());
    for lit in &q.lits {
        assert!(lit.len() <= MAX_LIT_BYTES, "interpolation: {}", lit.len());
    }
}
