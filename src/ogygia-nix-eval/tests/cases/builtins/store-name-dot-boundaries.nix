map
  (name: [
    (builtins.toFile name ".")
    (builtins.path { inherit name; path = ../fixtures/x; })
    ((derivation { inherit name; system = "x"; builder = "/bin/sh"; }).drvPath)
    (builtins.getContext (builtins.appendContext "x" {
      ${builtins.unsafeDiscardStringContext (builtins.toFile name ".")}.path = true;
    }))
  ])
  [ "..." "...-" ".x-" "..x-" "-" "-." "-.." "x-." "x-.." ]
