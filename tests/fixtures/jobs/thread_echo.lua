-- Reads what the spawner passed and returns a value: proves both directions
-- of the JSON hand-off, since a Lua value cannot cross between states.
return { got = args.label, doubled = args.n * 2 }
