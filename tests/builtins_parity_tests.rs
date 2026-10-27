//! Bash 5.2 parity for builtins: getopts, mapfile/readarray, umask, ulimit,
//! times, printf (-v, %q, %(fmt)T), read, type/command, test/[/[[ ]], the
//! directory stack, wait/kill, enable/hash and a few smaller ones.
//!
//! Every expected output below was produced by `bash --norc --noprofile -c`
//! (bash 5.2.21) with the same script, environment and fresh working directory;
//! bash itself never runs at test time. Only standard output and the exit
//! status are compared: diagnostics are bash-like but not byte-identical.

use std::path::Path;
use std::process::{Command, Stdio};

fn run_in(dir: &Path, path: &str, script: &str) -> (String, i32) {
    let output = Command::new(env!("CARGO_BIN_EXE_jsh"))
        .args(["--norc", "-c", script])
        .current_dir(dir)
        .env_clear()
        .env("PATH", path)
        .env("HOME", dir)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .output()
        .expect("spawn jsh");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

/// Run each `(script, stdout, status)` case in its own empty directory and
/// report every mismatch at once.
fn check(cases: &[(&str, &str, i32)]) {
    let mut failures = Vec::new();
    for (script, expected, status) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        let (stdout, code) = run_in(dir.path(), "/usr/local/bin:/usr/bin:/bin", script);
        if stdout != *expected || code != *status {
            failures.push(format!(
                "script: {script}\n  expected ({status}): {expected:?}\n  got      ({code}): {stdout:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn getopts_match_bash() {
    check(&[
        // getopts
        ("set -- -a -b val c; while getopts ab: o; do echo $o $OPTARG; done; shift $((OPTIND-1)); echo $1", "a\nb val\nc\n", 0),
        // getopts_clustered
        ("set -- -ab val rest; while getopts ab: o; do echo \"$o ${OPTARG-}\"; done; echo $OPTIND", "a \nb val\n3\n", 0),
        // getopts_attached_arg
        ("set -- -bval; while getopts b: o; do echo \"$o $OPTARG\"; done", "b val\n", 0),
        // getopts_unknown_noisy
        ("set -- -x; getopts ab o 2>/dev/null; echo \"$o [${OPTARG-unset}] st=$?\"", "? [unset] st=0\n", 0),
        // getopts_unknown_silent
        ("set -- -x; getopts :ab o; echo \"$o $OPTARG\"", "? x\n", 0),
        // getopts_missing_arg_silent
        ("set -- -b; getopts :b: o; echo \"$o $OPTARG\"", ": b\n", 0),
        // getopts_missing_arg_noisy
        ("set -- -b; getopts b: o 2>/dev/null; echo \"$o [${OPTARG-unset}]\"", "? [unset]\n", 0),
        // getopts_end
        ("set -- -a -- -b; while getopts ab o; do echo $o; done; shift $((OPTIND-1)); echo \"$@\"", "a\n-b\n", 0),
        // getopts_explicit_args
        ("while getopts xy: o -x -y 3; do echo $o ${OPTARG-}; done", "x\ny 3\n", 0),
        // getopts_in_function
        ("f() { local OPTIND o; while getopts n: o; do echo \"n=$OPTARG\"; done; }; f -n 1; f -n 2", "n=1\nn=2\n", 0),
        // getopts_opterr
        ("OPTERR=0; set -- -z; getopts a o; echo \"$o\"", "?\n", 0),
        // getopts_reset_and_reuse
        ("set -- -a -b; while getopts ab o; do echo $o; done; OPTIND=1; set -- -b; while getopts ab o; do echo again $o; done; echo $OPTIND", "a\nb\nagain b\n2\n", 0),
        // getopts_usage_errors
        ("getopts; echo st=$?; getopts a; echo st=$?; getopts a 1x 2>/dev/null; echo st=$?", "st=2\nst=2\nst=1\n", 0),
        // getopts_no_options
        ("set -- file; getopts a o; echo \"st=$? o=$o OPTIND=$OPTIND\"; set -- -; OPTIND=1; getopts a o; echo \"st=$? o=$o OPTIND=$OPTIND\"", "st=1 o=? OPTIND=1\nst=1 o=? OPTIND=1\n", 0),
        // getopts_optarg_unset_after_flag
        ("set -- -a; OPTARG=stale; getopts a o; echo \"[${OPTARG-unset}] $o\"", "[unset] a\n", 0),
        // getopts_missing_then_more
        ("set -- -x -a; while getopts :a o; do echo \"$o ${OPTARG-}\"; done; echo $OPTIND", "? x\na \n3\n", 0),
        // getopts_many_positional
        ("set -- 1 2 3 4 5 6 7 8 9 10 -z; shift 10; getopts z o; echo $o $OPTIND", "z 2\n", 0),
    ]);
}

#[test]
fn mapfile_match_bash() {
    check(&[
        // mapfile
        ("mapfile -t lines < <(printf 'a\\nb\\nc\\n'); echo ${#lines[@]} ${lines[2]}", "3 c\n", 0),
        // mapfile_keep_newline
        ("mapfile lines < <(printf 'a\\nb\\n'); printf '[%s]' \"${lines[@]}\"; echo", "[a\n][b\n]\n", 0),
        // readarray
        ("readarray -t lines <<< $'x\\ny'; echo ${lines[@]}", "x y\n", 0),
        // mapfile_d
        ("mapfile -d , -t parts <<< \"a,b,c\"; echo ${#parts[@]} ${parts[0]} \"[${parts[2]}]\"", "3 a [c\n]\n", 0),
        // mapfile_s_n
        ("mapfile -t -s 1 -n 2 arr < <(printf '0\\n1\\n2\\n3\\n'); echo ${arr[@]}", "1 2\n", 0),
        // mapfile_O
        ("arr=(x y z); mapfile -t -O 1 arr < <(printf 'p\\nq\\n'); echo ${arr[@]}", "x p q\n", 0),
        // mapfile_default_name
        ("printf 'm1\\nm2\\n' | { mapfile -t; echo ${MAPFILE[1]}; }", "m2\n", 0),
        // mapfile_empty_input
        ("mapfile -t arr < /dev/null; echo ${#arr[@]}", "0\n", 0),
        // mapfile_callback
        ("mapfile -t -C 'echo cb' -c 2 a < <(printf '1\\n2\\n3\\n4\\n5\\n'); echo ${a[@]}", "cb 1 2\ncb 3 4\n1 2 3 4 5\n", 0),
        // mapfile_nul_delim
        ("mapfile -d '' -t z < <(printf 'a\\0b\\0'); echo ${#z[@]} ${z[1]}", "2 b\n", 0),
        // mapfile_no_trailing_newline
        ("mapfile x < <(printf 'a\\nb'); printf '[%s]' \"${x[@]}\"; echo", "[a\n][b]\n", 0),
        // mapfile_clears_array
        ("arr=(q w e r); mapfile -t arr <<< \"n\"; echo ${#arr[@]} ${arr[@]}", "1 n\n", 0),
        // mapfile_count_zero_and_skip_all
        ("mapfile -n 0 -t y < <(printf '1\\n2\\n'); echo ${#y[@]}; mapfile -t -n 1 -s 5 k < <(printf '1\\n2\\n'); echo ${#k[@]}", "2\n0\n", 0),
        // mapfile_errors
        ("mapfile -u 9 r 2>/dev/null; echo st=$?; mapfile -t 1bad <<< x 2>/dev/null; echo st=$?; mapfile -x 2>/dev/null; echo st=$?; mapfile -n -1 a <<< x 2>/dev/null; echo st=$?", "st=1\nst=1\nst=2\nst=1\n", 0),
        // mapfile_leaves_rest_in_file
        ("printf 'l1\\nl2\\nl3\\n' > f; { mapfile -t -n 1 first; read -r second; echo \"${first[0]}|$second\"; } < f", "l1|l2\n", 0),
        // mapfile_leaves_rest_in_pipe
        ("printf 'l1\\nl2\\nl3\\n' | { mapfile -t -n 1 first; read -r second; echo \"${first[0]}|$second\"; }", "l1|l2\n", 0),
        // readarray_origin
        ("a=(x y z); readarray -t -O 2 a < <(printf 'p\\nq\\n'); echo ${#a[@]} ${a[@]}", "4 x y p q\n", 0),
    ]);
}

#[test]
fn umask_match_bash() {
    check(&[
        // umask_cmd
        ("umask 022; umask", "0022\n", 0),
        // umask_S
        ("umask 022; umask -S", "u=rwx,g=rx,o=rx\n", 0),
        // umask_symbolic
        ("umask 077; umask u=rwx,g=rx,o=; umask", "0027\n", 0),
        // umask_p
        ("umask 027; umask -p", "umask 0027\n", 0),
        // umask_effect
        ("umask 077; touch f; stat -c %a f", "600\n", 0),
        // umask_more
        ("umask 7; umask; umask g+w; umask -S; umask o-x,a+r; umask -S; umask a=; umask -S; umask 0; umask go-wx; umask; umask +x; umask -S; umask =r; umask -S", "0007\nu=rwx,g=rwx,o=\nu=rwx,g=rwx,o=r\nu=,g=,o=\n0033\nu=rwx,g=rx,o=rx\nu=r,g=r,o=r\n", 0),
        // umask_errors
        ("umask 999 2>/dev/null; echo st=$?; umask zz 2>/dev/null; echo st=$?; umask u+X 2>/dev/null; echo st=$?; umask -x 2>/dev/null; echo st=$?; umask -S 027; echo st=$?; umask -p -S", "st=1\nst=1\nst=1\nst=2\nu=rwx,g=rx,o=\nst=0\numask -S u=rwx,g=rx,o=\n", 0),
        // umask_dir_effect
        ("umask 027; mkdir d; stat -c %a d; touch g; stat -c %a g", "750\n640\n", 0),
    ]);
}

#[test]
fn ulimit_times_match_bash() {
    check(&[
        // ulimit
        ("[[ $(ulimit -n) =~ ^([0-9]+|unlimited)$ ]] && echo ok", "ok\n", 0),
        // ulimit_set
        ("(ulimit -n 64; ulimit -n)", "64\n", 0),
        // ulimit_a
        ("ulimit -a | grep -c 'open files'", "1\n", 0),
        // ulimit_soft_hard
        ("[[ $(ulimit -Hn) =~ ^([0-9]+|unlimited)$ && $(ulimit -Sn) =~ ^([0-9]+|unlimited)$ ]] && echo ok", "ok\n", 0),
        // ulimit_hard_soft_words
        ("(ulimit -n 50; ulimit -n hard; ulimit -n; ulimit -c soft; echo st=$?)", "50\nst=0\n", 0),
        // ulimit_a_lines
        ("ulimit -a | sed 's/) .*/)/'", "real-time non-blocking time  (microseconds, -R)\ncore file size              (blocks, -c)\ndata seg size               (kbytes, -d)\nscheduling priority                 (-e)\nfile size                   (blocks, -f)\npending signals                     (-i)\nmax locked memory           (kbytes, -l)\nmax memory size             (kbytes, -m)\nopen files                          (-n)\npipe size                (512 bytes, -p)\nPOSIX message queues         (bytes, -q)\nreal-time priority                  (-r)\nstack size                  (kbytes, -s)\ncpu time                   (seconds, -t)\nmax user processes                  (-u)\nvirtual memory              (kbytes, -v)\nfile locks                          (-x)\n", 0),
        // times_builtin
        ("times | wc -l", "2\n", 0),
        // times_format
        ("times | sed 's/[0-9]/N/g'", "NmN.NNNs NmN.NNNs\nNmN.NNNs NmN.NNNs\n", 0),
    ]);
}

#[test]
fn printf_match_bash() {
    check(&[
        // printf_v
        ("printf -v out '%05d' 42; echo $out", "00042\n", 0),
        // printf_v_array
        ("declare -a arr; printf -v 'arr[1]' '%s' hi; echo ${arr[1]}", "hi\n", 0),
        // printf_v_repeat
        ("printf -v joined '%s,' a b c; echo $joined", "a,b,c,\n", 0),
        // printf_v_invalid
        ("printf -v 1bad '%s' x; echo st=$?", "st=2\n", 0),
        // printf_q
        ("printf '%q\\n' \"a b\" \"it's\" '$x' '' 'plain' $'tab\\there' 'é'", "a\\ b\nit\\'s\n\\$x\n''\nplain\n$'tab\\there'\né\n", 0),
        // printf_q_width
        ("printf '[%10q]\\n' 'a b'", "[      a\\ b]\n", 0),
        // printf_T
        ("printf '%(%Y)T\\n' 0", "1970\n", 0),
        // printf_T_default_epoch
        ("TZ=UTC printf '%(%Y-%m-%d %H:%M:%S)T\\n' 86400", "1970-01-02 00:00:00\n", 0),
        // printf_T_now
        ("[[ $(printf '%(%Y)T' -1) =~ ^20[0-9][0-9]$ ]] && echo now-ok", "now-ok\n", 0),
        // printf_T_width
        ("TZ=UTC printf '[%12(%H:%M)T]\\n' 3600", "[       01:00]\n", 0),
        // printf_v_more
        ("printf -v x '%s-%s|' a b c; echo \"$x\"; printf -vy '%d' 7; echo $y; printf -v z '%b' 'a\\cb'; echo \"[$z]\"; printf -v w -- '-%s' q; echo $w", "a-b|c-|\n7\n[a]\n-q\n", 0),
        // printf_v_errors
        ("printf -v 2>/dev/null; echo st=$?; printf -x 2>/dev/null; echo st=$?; printf -v 'a b' x 2>/dev/null; echo st=$?", "st=2\nst=2\nst=2\n", 0),
        // printf_q_matrix
        ("printf '%q\\n' \"a b\" \"it's\" '$x' \"\" plain $'tab\\there' é 'a,b' '~x' 'x~y' 'a=~b' '#c' 'c#' '!x' '%^_+-./:=@' $'\\x01\\x7f' $'nl\\n' \"a'b c\" '[]{}|&;<>()*?\\`\"' $'\\e\\a\\v\\b\\f\\r' 日本 'a:~b'", "a\\ b\nit\\'s\n\\$x\n''\nplain\n$'tab\\there'\né\na\\,b\n\\~x\nx~y\na=\\~b\n\\#c\nc#\n\\!x\n%\\^_+-./:=@\n$'\\001\\177'\n$'nl\\n'\na\\'b\\ c\n\\[\\]\\{\\}\\|\\&\\;\\<\\>\\(\\)\\*\\?\\\\\\`\\\"\n$'\\E\\a\\v\\b\\f\\r'\n日本\na:\\~b\n", 0),
        // printf_q_reuse
        ("x=$(printf '%q' $'a b\\t\"c\"'); eval \"y=$x\"; [[ $y == $'a b\\t\"c\"' ]] && echo roundtrip", "roundtrip\n", 0),
        // printf_T_matrix
        ("export TZ=UTC; printf '[%12(%H:%M)T][%-8(%H)T][%.2(%Y)T]\\n' 3600 3600 0; printf '%()T|\\n' 0; printf '%(%Y-%m)T %s\\n' 0 x 86400 y; printf '%(%s)T\\n' 12345; printf '%5(%Y)T|%05(%Y)T|\\n' 0 0", "[       01:00][01      ][19]\n00:00:00|\n1970-01 x\n1970-01 y\n12345\n 1970| 1970|\n", 0),
        // printf_T_errors
        ("printf '%(%Y)T\\n' abc 2>/dev/null; echo st=$?; printf '%(%Y\\n' 0 >/dev/null 2>&1; echo st=$?", "1970\nst=1\nst=1\n", 0),
        // printf_T_shell_start
        ("[[ $(printf '%(%s)T' -2) -le $(printf '%(%s)T' -1) ]] && echo started-before-now", "started-before-now\n", 0),
    ]);
}

#[test]
fn read_match_bash() {
    check(&[
        // read_d_empty
        (
            "read -r -d '' v <<< \"multi\nline\"; echo \"$v\"",
            "multi\nline\n",
            0,
        ),
        // read_d_char
        ("read -r -d , v <<< \"a,b\"; echo $v", "a\n", 0),
        // read_n_chars
        ("read -n 2 v <<< \"héllo\"; echo $v", "hé\n", 0),
        // read_N_chars
        ("read -N 3 v <<< \"日本語x\"; echo $v", "日本語\n", 0),
        // read_last_gets_rest
        ("read -r a b <<< \"1 2 3 4\"; echo \"$b\"", "2 3 4\n", 0),
        // read_rest_preserves_inner_spacing
        (
            "read -r a b <<< \"1 2   3  4  \"; echo \"[$b]\"",
            "[2   3  4]\n",
            0,
        ),
        // read_ifs_default_trim
        ("read -r a <<< \"   lead   \"; echo \"[$a]\"", "[lead]\n", 0),
        // read_ifs_empty_no_trim
        (
            "IFS= read -r a <<< \"   lead   \"; echo \"[$a]\"",
            "[   lead   ]\n",
            0,
        ),
        // read_ifs_custom
        (
            "IFS=, read -r a b c <<< \"x,y,z\"; echo $a $b $c",
            "x y z\n",
            0,
        ),
        // read_ifs_colon_empty_fields
        (
            "IFS=: read -r a b c <<< \"x::z\"; echo \"[$a][$b][$c]\"",
            "[x][][z]\n",
            0,
        ),
        // read_fewer_fields
        (
            "read -r a b c <<< \"only\"; echo \"[$a][$b][$c]\"",
            "[only][][]\n",
            0,
        ),
        // read_backslash
        ("read v <<< 'a\\ b\\\\c'; echo \"$v\"", "a b\\c\n", 0),
        // read_backslash_r
        ("read -r v <<< 'a\\ b\\\\c'; echo \"$v\"", "a\\ b\\\\c\n", 0),
        // read_line_continuation
        (
            "printf 'one \\\\\\ntwo\\n' | { read v; echo \"$v\"; }",
            "one two\n",
            0,
        ),
        // read_a
        (
            "read -r -a arr <<< \"a b c\"; echo ${arr[2]} ${#arr[@]}",
            "c 3\n",
            0,
        ),
        // read_a_ifs
        (
            "IFS=, read -r -a arr <<< \"1,2,,4\"; echo ${#arr[@]} \"[${arr[2]}]\"",
            "4 []\n",
            0,
        ),
        // read_no_name
        (
            "read <<< \"  reply value  \"; echo \"[$REPLY]\"",
            "[  reply value  ]\n",
            0,
        ),
        // read_eof_status
        ("read -r v < /dev/null; echo st=$? \"[$v]\"", "st=1 []\n", 0),
        // read_partial_last_line
        (
            "printf 'no-newline' | { read -r v; echo st=$? $v; }",
            "st=1 no-newline\n",
            0,
        ),
        // read_while_loop_last_line
        (
            "printf 'a\\nb' | while read -r l || [[ -n $l ]]; do echo \"<$l>\"; done",
            "<a>\n<b>\n",
            0,
        ),
        // read_p_noninteractive
        (
            "read -p 'prompt> ' v <<< \"x\" 2>/dev/null; echo $v",
            "x\n",
            0,
        ),
        // read_into_array_element
        ("read 'arr[2]' <<< hi; echo \"${arr[2]}\"", "hi\n", 0),
        // readm_1
        (
            "IFS=, read a b <<< \"x,y,\"; echo \"1[$a][$b]\"",
            "1[x][y]\n",
            0,
        ),
        // readm_2
        (
            "IFS=, read a b <<< \"x,y,,\"; echo \"2[$a][$b]\"",
            "2[x][y,,]\n",
            0,
        ),
        // readm_3
        (
            "IFS=, read a b <<< \"x,y,z,\"; echo \"3[$a][$b]\"",
            "3[x][y,z,]\n",
            0,
        ),
        // readm_4
        (
            "IFS=\", \" read a b <<< \"x , y , \"; echo \"4[$a][$b]\"",
            "4[x][y]\n",
            0,
        ),
        // readm_5
        ("IFS=, read a <<< \"x,\"; echo \"5[$a]\"", "5[x]\n", 0),
        // readm_6
        ("IFS=, read a <<< \"x,,\"; echo \"6[$a]\"", "6[x,,]\n", 0),
        // readm_7
        (
            "IFS=: read a b c <<< \"x::z\"; echo \"7[$a][$b][$c]\"",
            "7[x][][z]\n",
            0,
        ),
        // readm_8
        (
            "IFS=: read a b c <<< \":x\"; echo \"8[$a][$b][$c]\"",
            "8[][x][]\n",
            0,
        ),
        // readm_9
        (
            "read a b <<< 'x\\ y z'; echo \"9[$a][$b]\"",
            "9[x y][z]\n",
            0,
        ),
        // readm_10
        (
            "read -r a b <<< 'x\\ y z'; echo \"10[$a][$b]\"",
            "10[x\\][y z]\n",
            0,
        ),
        // readm_11
        (
            "read a <<< 'tab\\there'; echo \"11[$a]\"",
            "11[tabthere]\n",
            0,
        ),
        // readm_12
        (
            "read a <<< 'end\\'; echo \"12[$a] st=$?\"",
            "12[end] st=1\n",
            0,
        ),
        // readm_13
        (
            "printf 'a\\\\\\nb c\\n' | { read x y; echo \"13[$x][$y]\"; }",
            "13[ab][c]\n",
            0,
        ),
        // readm_14
        (
            "printf 'a\\\\\\nb c\\n' | { read -r x y; echo \"14[$x][$y]\"; }",
            "14[a\\][]\n",
            0,
        ),
        // readm_15
        ("read -n 3 a <<< 'a\\bcd'; echo \"15[$a]\"", "15[abc]\n", 0),
        // readm_16
        (
            "read -n 2 a b <<< 'x yz'; echo \"16[$a][$b]\"",
            "16[x][]\n",
            0,
        ),
        // readm_17
        (
            "read -N 3 a b <<< 'x yz'; echo \"17[$a][$b]\"",
            "17[x y][]\n",
            0,
        ),
        // readm_18
        (
            "read -N 4 a <<< $'ab\\ncd'; echo \"18[$a]\"",
            "18[ab\nc]\n",
            0,
        ),
        // readm_19
        ("read -N 3 a <<< ' x '; echo \"19[$a]\"", "19[ x ]\n", 0),
        // readm_20
        (
            "read <<< '  a\\ b  '; echo \"20[$REPLY]\"",
            "20[  a b  ]\n",
            0,
        ),
        // readm_21
        (
            "read -r <<< '  a\\ b  '; echo \"21[$REPLY]\"",
            "21[  a\\ b  ]\n",
            0,
        ),
        // readm_22
        (
            "IFS= read -r a <<< '  sp  '; echo \"22[$a]\"",
            "22[  sp  ]\n",
            0,
        ),
        // readm_23
        (
            "read -a arr <<< '  a  b\\ c  '; echo \"23 ${#arr[@]} [${arr[1]}]\"",
            "23 2 [b c]\n",
            0,
        ),
        // readm_24
        (
            "IFS=, read -a arr <<< ',a,,b,'; echo \"24 ${#arr[@]} [${arr[0]}][${arr[3]}]\"",
            "24 4 [][b]\n",
            0,
        ),
        // readm_25
        ("read 1bad <<< x; echo \"25 st=$?\"", "25 st=1\n", 0),
        // readm_26
        (
            "read -d , a b <<< 'p q,r'; echo \"26[$a][$b]\"",
            "26[p][q]\n",
            0,
        ),
        // readm_27
        (
            "printf 'no-eol' | { read v; echo \"27 st=$? [$v]\"; }",
            "27 st=1 [no-eol]\n",
            0,
        ),
        // readm_28
        (
            "read v < /dev/null; echo \"28 st=$? [${v-unset}]\"",
            "28 st=1 []\n",
            0,
        ),
        // readm_29
        (
            "v=old; read v < /dev/null; echo \"29 [${v-unset}]\"",
            "29 []\n",
            0,
        ),
        // readm_30
        (
            "read -n 0 v <<< abc; echo \"30 st=$? [$v]\"",
            "30 st=0 []\n",
            0,
        ),
        // readm_31
        ("read -t 0 <<< abc; echo \"31 st=$?\"", "31 st=0\n", 0),
        // readm_32
        (
            "IFS=$'\\n' read a b <<< 'x y'; echo \"32[$a][$b]\"",
            "32[x y][]\n",
            0,
        ),
        // readm_33
        (
            "read a b c <<< '   '; echo \"33[$a][$b][$c]\"",
            "33[][][]\n",
            0,
        ),
        // readm_34
        (
            "read 'arr2[1]' <<< hi; echo \"34[${arr2[1]}]\"",
            "34[hi]\n",
            0,
        ),
        // readm_35
        (
            "IFS=' ,' read a b c <<< 'x ,, y'; echo \"35[$a][$b][$c]\"",
            "35[x][][y]\n",
            0,
        ),
        // readm_36
        (
            "read -n 1 -d x a <<< 'xyz'; echo \"36[$a] st=$?\"",
            "36[] st=0\n",
            0,
        ),
        // readm_37
        ("read -N 2 a <<< 'é日x'; echo \"37[$a]\"", "37[é日]\n", 0),
        // readm_38
        (
            "read -n 4 a <<< 'ab\\\ncd'; echo \"38[$a]\"\nread -rn 2 a <<< 'é日x'; echo \"39[$a]\"",
            "38[abcd]\n39[é日]\n",
            0,
        ),
        // readn_1
        (
            "printf 'a\\\\' | { read v; echo \"1 st=$? [$v]\"; }",
            "1 st=1 [a]\n",
            0,
        ),
        // readn_2
        (
            "{ read 1bad; read y; echo \"2 [$y]\"; } <<< $'a\\nb'",
            "2 [a]\n",
            0,
        ),
        // readn_3
        (
            "printf 'a\\r\\n' | { read v; echo \"3 [${#v}]\"; }",
            "3 [2]\n",
            0,
        ),
        // readn_5
        ("read -p 'P> ' v <<< x; echo \"5 [$v]\"", "5 [x]\n", 0),
        // readn_6
        ("read -rd, a <<< 'x,y'; echo \"6 [$a]\"", "6 [x]\n", 0),
        // readn_7
        ("read -n2 a <<< 'xyz'; echo \"7 [$a]\"", "7 [xy]\n", 0),
        // readn_8
        ("read -ra arr <<< 'p q'; echo \"8 ${arr[1]}\"", "8 q\n", 0),
        // readn_9
        ("read -x v <<< x; echo \"9 st=$?\"", "9 st=2\n", 0),
        // readn_10
        ("read -n x v <<< x; echo \"10 st=$?\"", "10 st=1\n", 0),
        // readn_12
        (
            "exec 4< <(echo 'fd four'); read -u 4 v; echo \"12 [$v]\"",
            "12 [fd four]\n",
            0,
        ),
        // readn_13
        ("read -a 1bad <<< x; echo \"13 st=$?\"", "13 st=1\n", 0),
        // readn_14
        (
            "arr=(old values here); read -a arr <<< 'new'; echo \"14 ${#arr[@]} ${arr[0]}\"",
            "14 1 new\n",
            0,
        ),
        // readn_15
        (
            "read a b <<< $'x\\\\\\ny z'; echo \"15 [$a][$b]\"",
            "15 [xy][z]\n",
            0,
        ),
        // readn_16
        (
            "read -d '' -n 3 v < <(printf 'ab\\0cd'); echo \"16 [$v] st=$?\"",
            "16 [ab] st=0\n",
            0,
        ),
        // readn_17
        ("read v <<< ''; echo \"17 st=$? [$v]\"", "17 st=0 []\n", 0),
        // readn_18
        (
            "IFS= read -d '' -r v < <(printf ' a \\n b \\n'); echo \"18 [$v] st=$?\"",
            "18 [ a \n b \n] st=1\n",
            0,
        ),
        // readn_19
        (
            "read -N 0 v <<< abc; echo \"19 st=$? [$v]\"",
            "19 st=0 []\n",
            0,
        ),
        // readn_20
        ("read -n -1 v <<< abc; echo \"20 st=$?\"", "20 st=1\n", 0),
        // readn_21
        (
            "read -d ab v <<< 'xbyaz'; echo \"21 [$v]\"",
            "21 [xby]\n",
            0,
        ),
        // readn_22
        (
            "read a b c d <<< 'w x'; echo \"22 [$a][$b][$c][$d]\"",
            "22 [w][x][][]\n",
            0,
        ),
        // readn_23
        ("x=1; read x <<< ''; echo \"23 [$x]\"", "23 []\n", 0),
    ]);
}

#[test]
fn type_command_match_bash() {
    check(&[
        // type_fn_case_extglob
        ("f() { case $1 in a|b) echo ab;; *.txt) echo txt;; esac; }; type f", "f is a function\nf () \n{ \n    case $1 in \n        a | b)\n            echo ab\n        ;;\n        *.txt)\n            echo txt\n        ;;\n    esac\n}\n", 0),
        // type_fn_redirects
        ("f() { echo a >out 2>&1; cat <in >>log; echo b >&2; exec 3>&-; }; type f", "f is a function\nf () \n{ \n    echo a > out 2>&1;\n    cat < in >> log;\n    echo b 1>&2;\n    exec 3>&-\n}\n", 0),
        // type_fn_nested_loops
        ("f() { for i in 1 2; do while true; do break 2; done; done; until false; do return 3; done; }; type f", "f is a function\nf () \n{ \n    for i in 1 2;\n    do\n        while true; do\n            break 2;\n        done;\n    done;\n    until false; do\n        return 3;\n    done\n}\n", 0),
        // type_fn_heredoc
        ("f() { cat <<EOF\nline $x\nEOF\n}; type f", "f is a function\nf () \n{ \n    cat <<EOF\nline $x\nEOF\n\n}\n", 0),
        // type_fn_quotes
        ("f() { echo \"a b\" 'c d' \"x\\\"y\" '$z' \"${arr[@]}\" \"$#\" ${v:-d}; }; type f", "f is a function\nf () \n{ \n    echo \"a b\" 'c d' \"x\\\"y\" '$z' \"${arr[@]}\" \"$#\" ${v:-d}\n}\n", 0),
        // type_fn_pipeline_neg
        ("f() { ! grep -q x file | wc -l && echo ok || echo no; }; type f", "f is a function\nf () \n{ \n    ! grep -q x file | wc -l && echo ok || echo no\n}\n", 0),
        // type_fn_subshell_bg
        ("f() { ( cd /; ls ) & wait; }; type f", "f is a function\nf () \n{ \n    ( cd /;\n    ls ) & wait\n}\n", 0),
        // type_fn_arith
        ("f() { (( i = 1 + 2 )); for ((j=0; j<3; j++)); do :; done; echo $(( j * 2 )); }; type f", "f is a function\nf () \n{ \n    (( i = 1 + 2 ));\n    for ((j=0; j<3; j++))\n    do\n        :;\n    done;\n    echo $(( j * 2 ))\n}\n", 0),
        // type_fn_cond
        ("f() { [[ -n $a && $b == x* || ! -d $c ]] && [[ $s =~ ^[a-z]+$ ]]; }; type f", "f is a function\nf () \n{ \n    [[ -n $a && $b == x* || ! -d $c ]] && [[ $s =~ ^[a-z]+$ ]]\n}\n", 0),
        // type_fn_assign
        ("f() { local a=1 b; a+=x; arr=(1 2 \"3 4\"); arr[5]=y; declare -A m; export E=1; }; type f", "f is a function\nf () \n{ \n    local a=1 b;\n    a+=x;\n    arr=(1 2 \"3 4\");\n    arr[5]=y;\n    declare -A m;\n    export E=1\n}\n", 0),
        // type_fn_if_elif
        ("f() { if [ \"$1\" = a ]; then echo a; elif [ \"$1\" = b ]; then echo b; elif [ \"$1\" = c ]; then echo c; else echo z; fi; }; type f", "f is a function\nf () \n{ \n    if [ \"$1\" = a ]; then\n        echo a;\n    else\n        if [ \"$1\" = b ]; then\n            echo b;\n        else\n            if [ \"$1\" = c ]; then\n                echo c;\n            else\n                echo z;\n            fi;\n        fi;\n    fi\n}\n", 0),
        // type_fn_brace_redirect
        ("f() { { echo a; echo b; } 2>/dev/null | sort; }; type f", "f is a function\nf () \n{ \n    { \n        echo a;\n        echo b\n    } 2> /dev/null | sort\n}\n", 0),
        // type_fn_empty_body_stmt
        ("f() { :; }; type f; g() { true; false; }; type g", "f is a function\nf () \n{ \n    :\n}\ng is a function\ng () \n{ \n    true;\n    false\n}\n", 0),
        // type_t
        ("f() { :; }; alias a=b; type -t f; type -t echo; type -t ls; type -t if; type -t nosuchcmd; echo st=$?", "function\nbuiltin\nfile\nkeyword\nst=1\n", 0),
        // type_t_alias_noninteractive
        ("shopt -s expand_aliases; alias a=b; type -t a", "alias\n", 0),
        // type_a
        ("type -a true | head -1", "true is a shell builtin\n", 0),
        // type_function_body
        ("f() { echo hi; }; type f", "f is a function\nf () \n{ \n    echo hi\n}\n", 0),
        // type_keyword
        ("type if", "if is a shell keyword\n", 0),
        // type_function_complex
        ("g() { local x=1; if [[ $x == 1 ]]; then echo one; fi; for i in a b; do echo $i; done; }; type g", "g is a function\ng () \n{ \n    local x=1;\n    if [[ $x == 1 ]]; then\n        echo one;\n    fi;\n    for i in a b;\n    do\n        echo $i;\n    done\n}\n", 0),
        // command_V
        ("f() { :; }; command -V f | head -1; command -V echo", "f is a function\necho is a shell builtin\n", 0),
        // command_p
        ("command -p ls / >/dev/null && echo cp-ok", "cp-ok\n", 0),
        // type_alias_quote
        ("shopt -s expand_aliases; alias q=\"it's\"; type q; command -v q", "q is aliased to `it's'\nalias q='it'\\''s'\n", 0),
    ]);
}

#[test]
fn conditionals_match_bash() {
    check(&[
        // test_v
        ("x=1; [[ -v x ]] && echo set; [[ -v nope ]] || echo unset", "set\nunset\n", 0),
        // test_v_empty
        ("x=; [[ -v x ]] && echo set-empty", "set-empty\n", 0),
        // test_v_array
        ("arr=(a); [[ -v arr[0] ]] && echo y; [[ -v arr[3] ]] || echo n", "y\nn\n", 0),
        // test_v_assoc
        ("declare -A m=([k]=v); [[ -v m[k] ]] && echo yk; [[ -v m[z] ]] || echo nz", "yk\nnz\n", 0),
        // test_v_bracket
        ("x=1; [ -v x ] && echo bracket-set", "bracket-set\n", 0),
        // test_glob
        ("x=abc; [[ $x == a* ]] && echo glob; [[ $x == \"a*\" ]] || echo literal", "glob\nliteral\n", 0),
        // test_glob_var_quoted
        ("p='a*'; x=abc; [[ $x == $p ]] && echo unquoted-glob; [[ $x == \"$p\" ]] || echo quoted-literal", "unquoted-glob\nquoted-literal\n", 0),
        // test_glob_partial_quote
        ("x='a*c'; [[ $x == \"a*\"c ]] && echo partial", "partial\n", 0),
        // test_regex_quoted
        ("x='a.c'; [[ abc =~ \"a.c\" ]] || echo quoted-regex-literal; [[ abc =~ a.c ]] && echo regex", "quoted-regex-literal\nregex\n", 0),
        // test_o
        ("set -e; [[ -o errexit ]] && echo on; [[ -o nounset ]] || echo off", "on\noff\n", 0),
        // test_o_bracket
        ("set -u; [ -o nounset ] && echo on", "on\n", 0),
        // test_string_ops
        ("[[ abc > abb ]] && echo gt; [[ -z \"\" && -n x ]] && echo zn", "gt\nzn\n", 0),
        // test_numeric_compare
        ("[[ 010 -eq 8 ]] && echo octal-eq; [[ 1+1 -eq 2 ]] && echo arith-eq", "octal-eq\narith-eq\n", 0),
        // test_dash_operand_binary
        ("set -- -v; [[ $1 == -v ]] && echo ok1; [[ $1 == -* ]] && echo ok2; x=-n; [[ $x = -n ]] && echo ok3", "ok1\nok2\nok3\n", 0),
        // test_short_circuit
        ("x=0; [[ 1 -eq 1 || x++ -eq 0 ]]; echo $x; [[ 1 -eq 2 && x++ -eq 0 ]]; echo $x; [[ 1 -eq 1 && x++ -eq 0 ]]; echo $x", "0\n0\n1\n", 0),
        // test_arith_error
        ("[[ 1/0 -eq 1 ]]; echo st=$?", "st=1\n", 0),
        // test_int_errors
        ("test x -eq 1; echo st=$?; [ \" 12 \" -eq 12 ]; echo st=$?; [ 010 -eq 8 ]; echo st=$?; [ -5 -lt +3 ]; echo st=$?", "st=2\nst=0\nst=1\nst=0\n", 0),
        // test_file_ops
        ("touch a; sleep 0.02; touch b; [[ b -nt a ]] && echo nt; [[ a -ot b ]] && echo ot; [[ a -ef a ]] && echo ef; [ b -nt nosuch ] && echo nt2; ln -s a l; [ -h l ] && echo h; [ -a a ] && echo a; [[ -L l ]] && echo L", "nt\not\nef\nnt2\nh\na\nL\n", 0),
        // test_regex_partial_quote
        ("[[ a.c =~ ^\"a.c\"$ ]] && echo y1; [[ abc =~ ^\"a.c\"$ ]] || echo n1; re='a.c'; [[ abc =~ $re ]] && echo y2; [[ abc =~ \"$re\" ]] || echo n2; [[ abc =~ ^a(b)c$ ]] && echo ${BASH_REMATCH[1]}", "y1\nn1\ny2\nn2\nb\n", 0),
        // test_glob_quoted_class
        ("[[ b == [abc] ]] && echo cls; [[ b == \"[abc]\" ]] || echo lit; [[ '[abc]' == \"[abc]\" ]] && echo litm; [[ 'a\\b' == \"a\\b\" ]] && echo bs", "cls\nlit\nlitm\nbs\n", 0),
        // test_not_equal_quoted
        ("x='a*'; [[ $x != \"a*\" ]] || echo eq; [[ abc != \"a*\" ]] && echo ne; [[ abc != a* ]] || echo globeq", "eq\nne\nglobeq\n", 0),
        // test_v_positional_negative
        ("set -- a b; [[ -v 2 ]] && echo p2; [[ -v 3 ]] || echo np3; arr=(x y z); [[ -v arr[-1] ]] && echo neg; [[ -v arr[@] ]] && echo all; i=1; [[ -v arr[i] ]] && echo ai; [[ -v arr ]] && echo a0; s=str; [[ -v s[0] ]] && echo s0", "p2\nnp3\nneg\nall\nai\na0\ns0\n", 0),
        // test_o_unknown
        ("[[ -o nosuchopt ]]; echo st=$?; [ -o pipefail ]; echo st=$?; set -o pipefail; test -o pipefail && echo pf", "st=1\nst=1\npf\n", 0),
        // test_bracket_paren_and_or
        ("[ \\( -n a -a -z \"\" \\) -o -n \"\" ] && echo ok; test ! -z x && echo nz", "ok\nnz\n", 0),
        // test_extglob_quoted
        ("shopt -s extglob; [[ ab == @(ab|cd) ]] && echo m1; [[ '@(ab|cd)' == \"@(ab|cd)\" ]] && echo m2; [[ ab == \"@(ab|cd)\" ]] || echo m3", "m1\nm2\nm3\n", 0),
        // test_cond_string_compare_ops
        ("[[ b > a && a < b ]] && echo lt; [[ -n \"\" || -z \"\" ]] && echo or; [[ ! ( -n \"\" ) ]] && echo grp", "lt\nor\ngrp\n", 0),
    ]);
}

#[test]
fn directories_match_bash() {
    check(&[
        // dirs_v
        ("cd /; pushd /tmp >/dev/null; dirs -v", " 0  /tmp\n 1  /\n", 0),
        // dirs_p
        ("cd /; pushd /tmp >/dev/null; dirs -p", "/tmp\n/\n", 0),
        // dirs_clear
        ("cd /; pushd /tmp >/dev/null; dirs -c; dirs", "/tmp\n", 0),
        // pushd_rotate
        ("cd /; pushd /tmp >/dev/null; pushd /usr >/dev/null; pushd +1 >/dev/null; pwd; dirs", "/tmp\n/tmp / /usr\n", 0),
        // popd_n
        ("cd /; pushd /tmp >/dev/null; pushd /usr >/dev/null; popd -n >/dev/null; dirs", "/usr /\n", 0),
        // pushd_noarg_swap
        ("cd /; pushd /tmp >/dev/null; pushd >/dev/null; pwd", "/\n", 0),
        // cd_dash_prints
        ("cd /tmp; cd /; cd -", "/tmp\n", 0),
        // pwd_P
        ("mkdir -p real; ln -s real link; cd link; pwd | sed 's|.*/||'; pwd -P | sed 's|.*/||'", "link\nreal\n", 0),
        // dirs_full_sequence
        ("cd /; pushd /tmp; pushd /usr; pushd /etc; dirs -v; dirs -l; dirs +1; dirs -1; dirs +9; echo st=$?; pushd +1; pushd -0; pushd; popd +1; popd -0; dirs; popd -n; dirs; popd; popd; echo st=$?", "/tmp /\n/usr /tmp /\n/etc /usr /tmp /\n 0  /etc\n 1  /usr\n 2  /tmp\n 3  /\n/etc /usr /tmp /\n/usr\n/tmp\nst=1\n/usr /tmp / /etc\n/etc /usr /tmp /\n/usr /etc /tmp /\n/usr /tmp /\n/usr /tmp\n/usr /tmp\n/usr\n/usr\nst=1\n", 0),
        // dirs_home_tilde
        ("cd; dirs; pushd -n /tmp >/dev/null; dirs; pwd | sed \"s|^$HOME|H|\"", "~\n~ /tmp\nH\n", 0),
        // pushd_errors
        ("pushd; echo st=$?; pushd /nonexistent 2>/dev/null; echo st=$?; popd 2>/dev/null; echo st=$?", "st=1\nst=1\nst=1\n", 0),
        // cd_edge_cases
        ("cd \"\"; echo st=$?; cd /tmp /usr 2>/dev/null; echo st=$?; mkdir -p cdp/sub; CDPATH=$PWD/cdp; cd sub | sed 's|.*/||'; cd sub >/dev/null; pwd | sed 's|.*/||'; cd /; unset HOME; cd 2>/dev/null; echo st=$?", "st=0\nst=1\nsub\nsub\nst=1\n", 0),
        // cd_logical_dotdot
        ("mkdir -p top/real/inner; cd top; ln -s real/inner link; cd link; cd ..; pwd | sed 's|.*/||'; cd link; cd -P ..; echo \"${PWD##*/}\"", "top\nreal\n", 0),
        // cd_oldpwd
        ("cd /tmp; cd /; echo $OLDPWD; cd - >/dev/null; echo $PWD $OLDPWD", "/tmp\n/tmp /\n", 0),
        // pwd_options
        ("mkdir -p r; ln -s r l; cd l; pwd -L | sed 's|.*/||'; pwd -P | sed 's|.*/||'; set -P; pwd | sed 's|.*/||'", "l\nr\nr\n", 0),
    ]);
}

#[test]
fn jobs_signals_match_bash() {
    check(&[
        // wait_n
        ("(exit 3) & (sleep 0.3; exit 4) & wait -n; echo st=$?", "st=3\n", 0),
        // wait_multiple
        ("(exit 2) & p1=$!; (exit 5) & p2=$!; wait $p1; a=$?; wait $p2; echo $a $?", "2 5\n", 0),
        // wait_all_status
        ("(exit 4) & wait; echo st=$?", "st=0\n", 0),
        // kill_l
        ("kill -l 9; kill -l | grep -c SIGKILL", "KILL\n1\n", 0),
        // kill_jobspec
        ("sleep 5 & kill %1; wait; echo done", "done\n", 0),
        // kill_list_format
        ("kill -l | head -2; kill -l | tail -1 | cat -A; kill -l 137 KILL sigterm 0; kill -l 200 2>/dev/null; echo st=$?", " 1) SIGHUP\t 2) SIGINT\t 3) SIGQUIT\t 4) SIGILL\t 5) SIGTRAP\n 6) SIGABRT\t 7) SIGBUS\t 8) SIGFPE\t 9) SIGKILL\t10) SIGUSR1\n63) SIGRTMAX-1^I64) SIGRTMAX^I$\nKILL\n9\n15\nEXIT\nst=1\n", 0),
        // kill_errors
        ("kill 2>/dev/null; echo st=$?; kill -s 2>/dev/null; echo st=$?; kill 999999 2>/dev/null; echo st=$?; kill -s FOO 1 2>/dev/null; echo st=$?; kill %9 2>/dev/null; echo st=$?; kill -0 $$; echo st=$?; kill abc 2>/dev/null; echo st=$?", "st=2\nst=1\nst=1\nst=1\nst=1\nst=0\nst=1\n", 0),
        // kill_signal_forms
        ("sleep 5 & sleep 0.2; kill -9 $!; wait $!; echo st=$?; sleep 5 & sleep 0.2; kill -n 15 $!; wait $!; echo st=$?; sleep 5 & sleep 0.2; kill -SIGUSR1 $!; wait $!; echo st=$?; sleep 5 & sleep 0.2; kill -sHUP $!; wait $!; echo st=$?", "st=137\nst=143\nst=138\nst=129\n", 0),
        // wait_more
        ("(exit 5) & p=$!; sleep 0.2; wait $p; echo st=$?; wait $p; echo st=$?; wait 999999 2>/dev/null; echo st=$?; wait %5 2>/dev/null; echo st=$?; wait abc 2>/dev/null; echo st=$?", "st=5\nst=5\nst=127\nst=127\nst=1\n", 0),
        // wait_n_p
        ("(sleep 0.1; exit 7) & (exit 2) & wait -n -p who; echo st=$? set=${who:+yes}; wait", "st=2 set=yes\n", 0),
        // wait_signaled
        ("(kill -TERM $BASHPID 2>/dev/null || kill -TERM $$) & wait $!; echo st=$?", "st=143\n", 0),
    ]);
}

#[test]
fn scripts_match_bash() {
    check(&[
        // script_arg_parser
        ("usage() { echo \"usage\"; }; parse() { local OPTIND opt verbose=0 out=; while getopts \":vo:h\" opt; do case $opt in v) verbose=$((verbose+1));; o) out=$OPTARG;; h) usage; return 0;; :) echo \"missing arg for -$OPTARG\"; return 2;; \\?) echo \"bad option -$OPTARG\"; return 2;; esac; done; shift $((OPTIND-1)); echo \"v=$verbose out=$out rest=$*\"; }; parse -vv -o file a b; parse -x; parse -o; parse -h", "v=2 out=file rest=a b\nbad option -x\nmissing arg for -o\nusage\n", 0),
        // script_csv_loop
        ("printf 'name,age\\nann,30\\nbob,25\\n' > data.csv; { read -r header; while IFS=, read -r name age; do printf '%-5s|%3d\\n' \"$name\" \"$age\"; done; } < data.csv", "ann  | 30\nbob  | 25\n", 0),
        // script_read_kv
        ("while IFS='=' read -r key value; do [[ -z $key || $key == \\#* ]] && continue; printf '%s -> [%s]\\n' \"$key\" \"$value\"; done <<'EOF'\n# comment\na=1\nb = two words\nc==eq\nEOF", "a -> [1]\nb  -> [ two words]\nc -> [=eq]\n", 0),
        // script_printf_table
        ("printf -v line '%*s' 10 ''; echo \"${line// /-}\"; printf '%-6s%6s\\n' a 1 bb 22; printf '%q ' \"x y\" z; echo", "----------\na          1\nbb        22\nx\\ y z \n", 0),
        // script_umask_subshell
        ("umask 022; (umask 077; umask); umask", "0077\n0022\n", 0),
        // script_test_file_checks
        ("touch f; mkdir d; [ -f f -a -d d ] && echo both; [ ! -e nope ] && echo noexist; [[ -s f ]] || echo empty; echo x > f; [[ -s f ]] && echo nonempty", "both\nnoexist\nempty\nnonempty\n", 0),
    ]);
}

#[test]
fn enable_hash_misc_match_bash() {
    check(&[
        // enable_cmd
        ("enable -n echo 2>/dev/null; type -t echo", "file\n", 0),
        // enable_list
        ("enable -a | grep -c 'enable echo'", "1\n", 0),
        // enable_more
        ("enable -n echo; enable -n | head -3; type -t echo; enable echo; type -t echo; enable nosuch 2>/dev/null; echo st=$?; enable -s | head -3; enable | grep -c '^enable echo$'", "enable -n echo\nfile\nbuiltin\nst=1\nenable .\nenable :\nenable break\n1\n", 0),
        // enable_disabled_runs_file
        ("enable -n echo; echo hi there; builtin echo x 2>/dev/null; echo st=$?", "hi there\nst=1\n", 0),
        // hash_cmd
        ("hash ls; hash | grep -c ls", "1\n", 0),
        // compgen_A_function
        ("myfn1() { :; }; myfn2() { :; }; compgen -A function myfn", "myfn1\nmyfn2\n", 0),
        // compgen_v
        ("myvar1=1; myvar2=2; compgen -v myvar", "myvar1\nmyvar2\n", 0),
        // compgen_more
        ("compgen -W 'apple banana avocado' a; compgen -A function; f1() { :; }; compgen -A function; compgen -k | head -3; compgen -b | head -3", "apple\navocado\nf1\nif\nthen\nelse\n.\n:\n[\n", 0),
        // echo_dash_e_combined
        ("echo -ne 'a\\tb\\n'", "a\tb\n", 0),
        // echo_dashdash
        ("echo -- -n", "-- -n\n", 0),
        // echo_more
        ("echo -n -e 'a\\tb'; echo; echo -E 'a\\tb'; echo -nE x; echo; echo -en; echo -x; echo -- -- -n; echo -e 'a\\cb'; echo 'x'", "a\tb\na\\tb\nx\n-x\n-- -- -n\nax\n", 0),
        // alias_listing
        ("alias zz='echo z' aa='ls -l' q=\"it's\"; alias; alias aa; alias nosuch; echo st=$?", "alias aa='ls -l'\nalias q='it'\\''s'\nalias zz='echo z'\nalias aa='ls -l'\nst=1\n", 0),
        // unalias_errors
        ("unalias nosuch; echo st=$?; alias a=b; unalias -a; alias; echo st=$?", "st=1\nst=0\n", 0),
        // shift_errors
        ("set -- a b; shift 3; echo st=$? $#; shift -1; echo st=$?; shift x; echo st=$?", "st=1 2\nst=1\nst=1\n", 0),
        // echo_edge
        ("echo -e 'x\\x41\\0101é'; echo -e '\\c'; echo -n; echo -e -n 'a\\n'; echo -", "xAAé\na\n-\n", 0),
        // true_false_colon_args
        ("true --help; echo $?; false x; echo $?; : a b c; echo $?", "0\n1\n0\n", 0),
        // command_builtin_misc
        ("command; echo st=$?; builtin; echo st=$?; builtin nosuch; echo st=$?", "st=0\nst=0\nst=1\n", 0),
    ]);
}

/// A PATH made of two private directories that both hold `mycmd`, and the
/// first one also an `echo`, so lookups have known answers on any system.
fn private_path(dir: &Path) -> (String, String, String) {
    use std::os::unix::fs::PermissionsExt;
    let first = dir.join("bin1");
    let second = dir.join("bin2");
    for (bin, names) in [(&first, &["mycmd", "echo"][..]), (&second, &["mycmd"][..])] {
        std::fs::create_dir(bin).unwrap();
        for name in names {
            let file = bin.join(name);
            std::fs::write(&file, format!("#!/bin/sh\nprintf '{name} from %s:' \"${{0%/*}}\"\nprintf ' %s' \"$@\"\nprintf '\\n'\n")).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let first = first.to_string_lossy().into_owned();
    let second = second.to_string_lossy().into_owned();
    let path = format!("{first}:{second}:/usr/bin:/bin");
    (path, first, second)
}

#[test]
fn type_and_command_search_path_like_bash() {
    let dir = tempfile::tempdir().unwrap();
    let (path, first, second) = private_path(dir.path());
    let (stdout, status) = run_in(
        dir.path(),
        &path,
        "f() { :; }; type -P mycmd; type -p mycmd; type -p echo; echo st=$?; \
         type -a mycmd; type -ap mycmd; type -t mycmd f echo; command -v mycmd; \
         command -V mycmd; type -P nosuch; echo st=$?",
    );
    assert_eq!(status, 0);
    assert_eq!(
        stdout,
        format!(
            "{first}/mycmd\n{first}/mycmd\nst=0\nmycmd is {first}/mycmd\nmycmd is {second}/mycmd\n\
             {first}/mycmd\n{second}/mycmd\nfile\nfunction\nbuiltin\n{first}/mycmd\n\
             mycmd is {first}/mycmd\nst=1\n"
        )
    );
}

#[test]
fn hash_lists_remembered_locations_like_bash() {
    let dir = tempfile::tempdir().unwrap();
    let (path, first, _) = private_path(dir.path());
    let (stdout, status) = run_in(
        dir.path(),
        &path,
        "hash; hash mycmd; hash; hash -t mycmd; hash -l; hash -d mycmd; hash; \
         hash -p /x/y alias1; hash -t alias1; hash -r; hash; hash nosuch 2>/dev/null; echo st=$?",
    );
    assert_eq!(status, 0);
    assert_eq!(
        stdout,
        format!(
            "hash: hash table empty\nhits\tcommand\n   0\t{first}/mycmd\n{first}/mycmd\n\
             builtin hash -p {first}/mycmd mycmd\nhash: hash table empty\n/x/y\n\
             hash: hash table empty\nst=1\n"
        )
    );
}

#[test]
fn a_disabled_builtin_name_runs_the_command_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let (path, first, _) = private_path(dir.path());
    let (stdout, status) = run_in(
        dir.path(),
        &path,
        "enable -n echo; echo hi there; type -t echo; command echo via command; \
         builtin echo x 2>/dev/null; echo st=$?; enable echo; echo back",
    );
    assert_eq!(status, 0);
    assert_eq!(
        stdout,
        format!(
            "echo from {first}: hi there\nfile\necho from {first}: via command\n\
             echo from {first}: st=1\nback\n"
        )
    );
}

#[test]
fn command_p_searches_the_default_path_not_path() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _, _) = private_path(dir.path());
    // `mycmd` exists only on $PATH, `ls` only on the system default path.
    let (stdout, _) = run_in(
        dir.path(),
        &path,
        "command -p mycmd 2>/dev/null; echo st=$?; command -pv mycmd; echo st=$?; \
         PATH=/nowhere; command -p ls / >/dev/null && echo found-ls",
    );
    assert_eq!(stdout, "st=127\nst=1\nfound-ls\n");
}

#[test]
fn printf_v_output_is_bounded() {
    // 2 * 40 MiB fields exceed the 64 MiB limit on what -v may collect.
    let dir = tempfile::tempdir().unwrap();
    let (stdout, status) = run_in(
        dir.path(),
        "/usr/bin:/bin",
        "printf -v big '%1048576s' $(seq 80) 2>/dev/null; echo st=$? ${#big}",
    );
    assert_eq!(status, 0);
    assert_eq!(stdout, "st=1 0\n");
}
