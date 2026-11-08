let c = n: if n == 0 then "/dev/null" else { outPath = c (n - 1); }; in
"${c 9997}"
