/// Stream processing and utility commands
/// Adds functional programming style commands to jsh
/// sum - Add up all numbers in arguments
use std::io::IsTerminal;

pub fn builtin_sum(args: &[String]) -> i32 {
    let sum: f64 = args.iter().filter_map(|s| s.parse::<f64>().ok()).sum();
    if sum.fract() == 0.0 {
        println!("{}", sum as i64);
    } else {
        println!("{}", sum);
    }
    0
}

/// avg - Calculate average
pub fn builtin_avg(args: &[String]) -> i32 {
    if args.is_empty() {
        return 0;
    }
    let nums: Vec<f64> = args.iter().filter_map(|s| s.parse::<f64>().ok()).collect();

    if nums.is_empty() {
        return 0;
    }

    let avg = nums.iter().sum::<f64>() / nums.len() as f64;
    if avg.fract() == 0.0 {
        println!("{}", avg as i64);
    } else {
        println!("{}", avg);
    }
    0
}

/// min - Find minimum value
pub fn builtin_min(args: &[String]) -> i32 {
    let nums: Vec<f64> = args.iter().filter_map(|s| s.parse::<f64>().ok()).collect();

    if let Some(min) = nums
        .iter()
        .cloned()
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
        if min.fract() == 0.0 {
            println!("{}", min as i64);
        } else {
            println!("{}", min);
        }
    }
    0
}

/// max - Find maximum value
pub fn builtin_max(args: &[String]) -> i32 {
    let nums: Vec<f64> = args.iter().filter_map(|s| s.parse::<f64>().ok()).collect();

    if let Some(max) = nums
        .iter()
        .cloned()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    {
        if max.fract() == 0.0 {
            println!("{}", max as i64);
        } else {
            println!("{}", max);
        }
    }
    0
}

/// lines - Read stdin line by line (filter for non-empty lines)
pub fn builtin_lines(_args: &[String]) -> i32 {
    if std::io::stdin().is_terminal() {
        eprintln!("lines: requires piped input, e.g.: cat file | lines");
        return 1;
    }
    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if let Ok(line) = line {
            if !line.trim().is_empty() {
                println!("{}", line);
            }
        }
    }
    0
}

/// stats - Show cache and performance statistics
pub fn builtin_stats(_args: &[String]) -> i32 {
    println!("=== jsh Performance Stats ===");

    // Parser cache stats
    if let Some(stats) = crate::parser::cache_stats() {
        println!("\nParser Cache:");
        println!("  Entries: {}", stats.entries);
        println!("  Total Size: {} bytes", stats.total_size);
        println!("  Total Hits: {}", stats.total_hits);
        if stats.entries > 0 {
            println!(
                "  Avg Hits/Entry: {:.2}",
                stats.total_hits as f64 / stats.entries as f64
            );
        }
    }

    println!("\nCompletion Cache:");
    println!("  (Use 'complete --stats' for details)");

    0
}

/// trim - Remove whitespace from start and end
pub fn builtin_trim(args: &[String]) -> i32 {
    for arg in args {
        println!("{}", arg.trim());
    }
    0
}

/// count - Count lines, words, characters
pub fn builtin_count(args: &[String]) -> i32 {
    if std::io::stdin().is_terminal() {
        eprintln!("count: requires piped input, e.g.: cat file | count");
        return 1;
    }
    use std::io::BufRead;

    let mut lines = 0;
    let mut words = 0;
    let mut chars = 0;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if let Ok(line) = line {
            lines += 1;
            words += line.split_whitespace().count();
            chars += line.chars().count();
        }
    }

    if args.is_empty() || args[0] == "-l" {
        println!("lines: {}", lines);
    }
    if args.is_empty() || args[0] == "-w" {
        println!("words: {}", words);
    }
    if args.is_empty() || args[0] == "-c" {
        println!("chars: {}", chars);
    }

    0
}

/// reverse - Reverse order of arguments or lines
pub fn builtin_reverse(args: &[String]) -> i32 {
    if args.is_empty() {
        if std::io::stdin().is_terminal() {
            eprintln!("reverse: requires piped input or arguments, e.g.: cat file | reverse");
            return 1;
        }
        // Read from stdin
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut lines: Vec<String> = stdin.lock().lines().filter_map(|l| l.ok()).collect();
        lines.reverse();
        for line in lines {
            println!("{}", line);
        }
    } else {
        // Reverse arguments
        let mut args = args.to_vec();
        args.reverse();
        for arg in args {
            println!("{}", arg);
        }
    }
    0
}

/// upper - Convert to uppercase
pub fn builtin_upper(args: &[String]) -> i32 {
    if args.is_empty() {
        if std::io::stdin().is_terminal() {
            eprintln!("upper: requires piped input or arguments, e.g.: upper hello");
            return 1;
        }
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(line) = line {
                println!("{}", line.to_uppercase());
            }
        }
    } else {
        for arg in args {
            println!("{}", arg.to_uppercase());
        }
    }
    0
}

/// lower - Convert to lowercase
pub fn builtin_lower(args: &[String]) -> i32 {
    if args.is_empty() {
        if std::io::stdin().is_terminal() {
            eprintln!("lower: requires piped input or arguments, e.g.: lower HELLO");
            return 1;
        }
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            if let Ok(line) = line {
                println!("{}", line.to_lowercase());
            }
        }
    } else {
        for arg in args {
            println!("{}", arg.to_lowercase());
        }
    }
    0
}
