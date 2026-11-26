# A Git URL classified as SCP-style must have a path after removing
# recognised parameters; this is an evaluation error, not a parse error.
builtins.parseFlakeRef "git+a:?ref=main&dir=d"
