let n = "c"; in rec { ${n} = 1; __overrides = { c = 3; }; }
