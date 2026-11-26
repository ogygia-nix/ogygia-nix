builtins.tryEval ((derivation { name = "..-x"; system = "x"; builder = "/bin/sh"; }).drvPath)
