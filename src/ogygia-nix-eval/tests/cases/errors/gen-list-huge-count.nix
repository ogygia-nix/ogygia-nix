# A count too large to allocate is an error that tryEval does not catch.
builtins.tryEval (builtins.genList (x: x) 1142857877083129900)
