# A URL whose scheme is file, perhaps after a '+', must not name a host.
builtins.tryEval (fetchGit {
  url = "x+file://h";
  rev = throw "x";
})
