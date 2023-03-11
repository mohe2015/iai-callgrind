use cfg_if::cfg_if;
use colored::{ColoredString, Colorize};
use core::panic;
use log::{debug, warn};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug)]
struct Args {
    executable: String,
    module: String,
    callgrind_args: Option<Vec<String>>,
}

impl Args {
    fn new(executable: &str, module: &str, callgrind_args: Vec<String>) -> Self {
        Self {
            executable: executable.to_string(),
            module: module.to_string(),
            callgrind_args: Some(callgrind_args),
        }
    }
}

fn check_valgrind() -> bool {
    let result = Command::new("valgrind")
        .arg("--tool=callgrind")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Err(e) => {
            eprintln!("Unexpected error while launching valgrind. Error: {}", e);
            false
        }
        Ok(status) => {
            if status.success() {
                true
            } else {
                eprintln!("Failed to launch valgrind. Error: {}. Please ensure that valgrind is installed and on the $PATH.", status);
                false
            }
        }
    }
}

fn get_arch() -> String {
    let output = Command::new("uname")
        .arg("-m")
        .stdout(Stdio::piped())
        .output()
        .expect("Failed to run `uname` to determine CPU architecture.");

    String::from_utf8(output.stdout)
        .expect("`-uname -m` returned invalid unicode.")
        .trim()
        .to_owned()
}

fn basic_valgrind() -> Command {
    Command::new("valgrind")
}

// Invoke Valgrind, disabling ASLR if possible because ASLR could noise up the results a bit
cfg_if! {
    if #[cfg(target_os = "linux")] {
        fn valgrind_without_aslr(arch: &str) -> Command {
            let mut cmd = Command::new("setarch");
            cmd.arg(arch)
                .arg("-R")
                .arg("valgrind");
            cmd
        }
    } else if #[cfg(target_os = "freebsd")] {
        fn valgrind_without_aslr(_arch: &str) -> Command {
            let mut cmd = Command::new("proccontrol");
            cmd.arg("-m")
                .arg("aslr")
                .arg("-s")
                .arg("disable");
            cmd
        }
    } else {
        fn valgrind_without_aslr(_arch: &str) -> Command {
            // Can't disable ASLR on this platform
            basic_valgrind()
        }
    }
}

#[derive(Debug)]
struct CallgrindArgs {
    i1: String,
    d1: String,
    ll: String,
    cache_sim: String,
    collect_atstart: String,
    other: Vec<String>,
    toggle_collect: Option<String>,
    compress_strings: String,
    compress_pos: String,
    callgrind_out_file: Option<String>,
}
impl Default for CallgrindArgs {
    fn default() -> Self {
        Self {
            i1: String::from("--I1=32768,8,64"),
            d1: String::from("--D1=32768,8,64"),
            ll: String::from("--LL=8388608,16,64"),
            cache_sim: String::from("--cache-sim=yes"),
            collect_atstart: String::from("--collect-atstart=no"),
            toggle_collect: Default::default(),
            compress_pos: String::from("--compress-pos=no"),
            compress_strings: String::from("--compress-strings=no"),
            callgrind_out_file: Default::default(),
            other: Default::default(),
        }
    }
}

impl CallgrindArgs {
    fn from_args(args: &Args) -> Self {
        let mut default = Self::default();
        for arg in args.callgrind_args.iter().flat_map(|a| a.iter()) {
            if arg.starts_with("--I1=") {
                default.i1 = arg.to_owned();
            } else if arg.starts_with("--D1=") {
                default.d1 = arg.to_owned();
            } else if arg.starts_with("--LL=") {
                default.ll = arg.to_owned();
            } else if arg.starts_with("--cache-sim=") {
                warn!("Ignoring callgrind argument: '{}'", arg);
            } else if arg.starts_with("--collect-atstart=") {
                default.collect_atstart = arg.to_owned();
            } else if arg.starts_with("--compress-strings=") {
                default.compress_strings = arg.to_owned();
            } else if arg.starts_with("--compress-pos=") {
                default.compress_pos = arg.to_owned();
            } else if arg.starts_with("--toggle-collect=") {
                warn!(
                    "The callgrind argument '{}' will overwrite the default setting and may produce wrong results.",
                    arg
                );
                default.toggle_collect = Some(arg.to_owned());
            } else if arg.starts_with("--callgrind-out-file=") {
                warn!("Ignoring callgrind argument: '{}'", arg);
            } else {
                default.other.push(arg.to_owned());
            }
        }
        default
    }

    fn parse_with(&self, output_file: &Path, module: &str, function_name: &str) -> Vec<String> {
        let mut args = vec![
            self.i1.clone(),
            self.d1.clone(),
            self.ll.clone(),
            self.cache_sim.clone(),
            self.collect_atstart.clone(),
            self.compress_strings.clone(),
            self.compress_pos.clone(),
        ];
        args.extend(self.other.iter().cloned());
        match &self.callgrind_out_file {
            Some(arg) => args.push(arg.clone()),
            None => args.push(format!("--callgrind-out-file={}", output_file.display())),
        }
        match &self.toggle_collect {
            Some(arg) => args.push(arg.clone()),
            None => args.push(format!("--toggle-collect=*{}::{}", module, function_name)),
        }

        args
    }
}

#[inline(never)]
fn run_bench(
    module: &str,
    arch: &str,
    executable: &str,
    index: usize,
    function_name: &str,
    allow_aslr: bool,
    callgrind_args: &CallgrindArgs,
) -> (CallgrindStats, Option<CallgrindStats>) {
    let mut cmd = if allow_aslr {
        basic_valgrind()
    } else {
        valgrind_without_aslr(arch)
    };

    let target = PathBuf::from("target/iai");
    let module_path: PathBuf = module.split("::").collect();
    let file_name = PathBuf::from(format!("callgrind.{}.out", function_name));

    let mut output_file = target;
    output_file.push(module_path);
    output_file.push(file_name);

    let old_file = output_file.with_extension("out.old");

    std::fs::create_dir_all(output_file.parent().unwrap()).expect("Failed to create directory");

    if output_file.exists() {
        // Already run this benchmark once; move last results to .old
        std::fs::copy(&output_file, &old_file).unwrap();
    }

    let callgrind_args = callgrind_args.parse_with(&output_file, module, function_name);
    debug!("Callgrind arguments: {}", callgrind_args.join(" "));
    let status = cmd
        // Set some reasonable cache sizes. The exact sizes matter less than having fixed sizes,
        // since otherwise callgrind would take them from the CPU and make benchmark runs
        // even more incomparable between machines.
        .arg("--tool=callgrind")
        .args(callgrind_args)
        .arg(executable)
        .arg("--iai-run")
        .arg(index.to_string())
        // Currently not used in iai-callgrind itself, but in `callgrind_annotate` this name is
        // shown and makes it easier to identify the benchmark under test
        .arg(format!("{}::{}", module, function_name))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("Failed to run benchmark");

    if !status.success() {
        panic!(
            "Failed to run benchmark in callgrind. Exit code: {}",
            status
        );
    }

    let new_stats = parse_callgrind_output(&output_file, module, function_name);
    let old_stats = if old_file.exists() {
        Some(parse_callgrind_output(&old_file, module, function_name))
    } else {
        None
    };

    (new_stats, old_stats)
}

// A curated sample output which this function must be able to parse to CallgrindStats.
// For more details see the format specification https://valgrind.org/docs/manual/cl-format.html
//
// # callgrind format
// # ... a lot of lines which we're not interested in
// fn=test_file::test_function
// 0 4 1 2 1 1 0 1
// cfn=some::library::function
// calls=1 0
// 0 3 1 0 1 0 0 1
// 0 12 3 6 1 0 0 1
// cfn=some::other::library::function
// calls=1 0
// 0 6789 593 463 72 37 18 72 37 18
// 0 4 2 0 1 0 0 1
//
// # the empty line above or the end of file ends the parsing
fn parse_callgrind_output(file: &Path, module: &str, function_name: &str) -> CallgrindStats {
    let sentinel = format!("fn={}", [module, function_name].join("::"));

    let file_in = File::open(file).expect("Unable to open callgrind output file");
    let mut iter = BufReader::new(file_in).lines().map(|l| l.unwrap());
    if !iter
        .by_ref()
        .find(|l| !l.trim().is_empty())
        .expect("Found empty file")
        .contains("callgrind format")
    {
        warn!("Missing file format specifier. Assuming callgrind format.");
    };

    // Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw
    let mut counters: [u64; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut is_recording = false;
    let mut has_counters = false;
    for line in iter {
        let line = line.trim_start();
        if !is_recording {
            if line.starts_with(&sentinel) {
                is_recording = true;
            }
            continue;
        }
        // We're only interested in the counters for function calls within the benchmark function and
        // ignore counters for the benchmark function itself.
        if is_recording && line.starts_with("cfn=") {
            has_counters = true;
            continue;
        }
        if has_counters && line.starts_with(|c: char| c.is_ascii_digit()) {
            // From the documentation of the callgrind format:
            // > If a cost line specifies less event counts than given in the "events" line, the
            // > rest is assumed to be zero.
            for (index, counter) in line
                .split_ascii_whitespace()
                // skip the first number which is just the line number
                .skip(1)
                .map(|s| s.parse::<u64>().expect("Encountered non ascii digit"))
                // we're only interested in the counters for instructions and the cache
                .take(9)
                .enumerate()
            {
                counters[index] += counter;
            }
        }
        if is_recording && line.is_empty() {
            break;
        }
    }

    CallgrindStats {
        l1_instructions_cache_reads: counters[0],
        total_data_cache_reads: counters[1],
        total_data_cache_writes: counters[2],
        l1_instructions_cache_read_misses: counters[3],
        l1_data_cache_read_misses: counters[4],
        l1_data_cache_write_misses: counters[5],
        l3_instructions_cache_misses: counters[6],
        l3_data_cache_read_misses: counters[7],
        l3_data_cache_write_misses: counters[8],
    }
}

#[derive(Clone, Debug)]
struct CallgrindStats {
    /// Ir: equals the number of instructions executed
    l1_instructions_cache_reads: u64,
    /// I1mr: I1 cache read misses
    l1_instructions_cache_read_misses: u64,
    /// ILmr: LL cache instruction read misses
    l3_instructions_cache_misses: u64,
    /// Dr: Memory reads
    total_data_cache_reads: u64,
    /// D1mr: D1 cache read misses
    l1_data_cache_read_misses: u64,
    /// DLmr: LL cache data read misses
    l3_data_cache_read_misses: u64,
    /// Dw: Memory writes
    total_data_cache_writes: u64,
    /// D1mw: D1 cache write misses
    l1_data_cache_write_misses: u64,
    /// DLmw: LL cache data write misses
    l3_data_cache_write_misses: u64,
}
impl CallgrindStats {
    fn summarize(&self) -> CallgrindSummary {
        let ram_hits = self.l3_instructions_cache_misses
            + self.l3_data_cache_read_misses
            + self.l3_data_cache_write_misses;
        let l1_data_accesses = self.l1_data_cache_read_misses + self.l1_data_cache_write_misses;
        let l1_miss = self.l1_instructions_cache_read_misses + l1_data_accesses;
        let l3_accesses = l1_miss;
        let l3_hits = l3_accesses - ram_hits;

        let total_memory_rw = self.l1_instructions_cache_reads
            + self.total_data_cache_reads
            + self.total_data_cache_writes;
        let l1_data_hits =
            total_memory_rw - self.l1_instructions_cache_reads - (ram_hits + l3_hits);
        assert!(
            total_memory_rw == l1_data_hits + self.l1_instructions_cache_reads + l3_hits + ram_hits
        );

        // Uses Itamar Turner-Trauring's formula from https://pythonspeed.com/articles/consistent-benchmarking-in-ci/
        let cycles =
            self.l1_instructions_cache_reads + l1_data_hits + (5 * l3_hits) + (35 * ram_hits);

        CallgrindSummary {
            l1_instructions: self.l1_instructions_cache_reads,
            l1_data_hits,
            l3_hits,
            ram_hits,
            total_memory_rw,
            cycles,
        }
    }

    fn signed_short(n: f64) -> String {
        let n_abs = n.abs();

        if n_abs < 10.0 {
            format!("{:+.6}", n)
        } else if n_abs < 100.0 {
            format!("{:+.5}", n)
        } else if n_abs < 1000.0 {
            format!("{:+.4}", n)
        } else if n_abs < 10000.0 {
            format!("{:+.3}", n)
        } else if n_abs < 100000.0 {
            format!("{:+.2}", n)
        } else if n_abs < 1000000.0 {
            format!("{:+.1}", n)
        } else {
            format!("{:+.0}", n)
        }
    }

    fn percentage_diff(new: u64, old: u64) -> ColoredString {
        fn format(string: ColoredString) -> ColoredString {
            ColoredString::from(format!(" ({})", string).as_str())
        }

        if new == old {
            return format("No Change".bright_black());
        }

        let new = new as f64;
        let old = old as f64;

        let diff = (new - old) / old;
        let pct = diff * 100.0;

        if pct.is_sign_positive() {
            format(
                format!("{:>+6}%", Self::signed_short(pct))
                    .bright_red()
                    .bold(),
            )
        } else {
            format(
                format!("{:>+6}%", Self::signed_short(pct))
                    .bright_green()
                    .bold(),
            )
        }
    }

    fn print(&self, old: Option<CallgrindStats>) {
        let summary = self.summarize();
        let old_summary = old.map(|stat| stat.summarize());
        println!(
            "  Instructions:     {:>15}{}",
            summary.l1_instructions.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l1_instructions, old.l1_instructions),
                None => String::new().normal(),
            }
        );
        println!(
            "  L1 Data Hits:     {:>15}{}",
            summary.l1_data_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l1_data_hits, old.l1_data_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  L2 Hits:          {:>15}{}",
            summary.l3_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.l3_hits, old.l3_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  RAM Hits:         {:>15}{}",
            summary.ram_hits.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.ram_hits, old.ram_hits),
                None => String::new().normal(),
            }
        );
        println!(
            "  Total read+write: {:>15}{}",
            summary.total_memory_rw.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.total_memory_rw, old.total_memory_rw),
                None => String::new().normal(),
            }
        );
        println!(
            "  Estimated Cycles: {:>15}{}",
            summary.cycles.to_string().bold(),
            match &old_summary {
                Some(old) => Self::percentage_diff(summary.cycles, old.cycles),
                None => String::new().normal(),
            }
        );
    }
}

#[derive(Clone, Debug)]
struct CallgrindSummary {
    l1_instructions: u64,
    l1_data_hits: u64,
    l3_hits: u64,
    ram_hits: u64,
    total_memory_rw: u64,
    cycles: u64,
}

pub fn run() {
    let mut args_iter = std::env::args().skip(1);
    let module = args_iter.next().unwrap();
    let mut benches = vec![];
    let executable = loop {
        if let Some(arg) = args_iter.next() {
            match arg.split_once('=') {
                Some((key, value)) if key == "--iai-bench" => benches.push(value.to_string()),
                Some(_) | None => break arg,
            }
        }
    };

    let mut args = args_iter
        .filter(|a| a.starts_with("--"))
        .collect::<Vec<String>>();
    if args.last().map_or(false, |a| a == "--bench") {
        args.pop();
    }
    let args = Args::new(&executable, &module, args);

    if !check_valgrind() {
        return;
    }

    let arch = get_arch();

    let allow_aslr = std::env::var_os("IAI_ALLOW_ASLR").is_some();

    let callgrind_args = CallgrindArgs::from_args(&args);
    for (index, name) in benches.iter().enumerate() {
        let (stats, old_stats) = run_bench(
            &args.module,
            &arch,
            &args.executable,
            index,
            name,
            allow_aslr,
            &callgrind_args,
        );

        println!("{}", format!("{}::{}", module, name).green());
        stats.print(old_stats);
    }
}
