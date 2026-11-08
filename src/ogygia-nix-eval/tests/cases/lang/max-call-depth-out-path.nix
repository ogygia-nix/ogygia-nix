let c = n: if n == 0 then "/dev/null" else { outPath = c (n - 1); }; in
[ (builtins.toPath (c 9995)) (builtins.pathExists (c 9995)) ]
