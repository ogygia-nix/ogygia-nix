{
  inherit (builtins) getFlake parseFlakeRef flakeRefToString;
  forcesRef = builtins.tryEval (builtins.flakeRefToString { type = throw "x"; });
  fetchTree = builtins ? fetchTree;
  globalFetchTree = with { fetchTree = "with"; }; fetchTree;
}
