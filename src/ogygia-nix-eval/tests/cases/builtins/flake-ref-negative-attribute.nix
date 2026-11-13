# Reject a negative integer before forcing the next attribute's throw.
builtins.tryEval (builtins.flakeRefToString { a = -1; b = throw ""; })
