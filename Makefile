# The gate. Anything under bin/ or deployments/ is used to decide what the pair runs and what its
# numbers mean, so it does not ship on "it worked when I ran it once".
.PHONY: test check sync

test:
	uv run --with pytest pytest tests -q

# What to run before committing a tool or deployment change, and before trusting a measurement.
check: test
	@python3 -c "import tomllib,pathlib,sys; [tomllib.load(p.open('rb')) for p in pathlib.Path('deployments').glob('*.toml')]; print('deployments parse')"
	@for f in bin/*; do [ -f "$$f" ] || continue; python3 -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" $$f || exit 1; done; echo "bin/ parses"

sync: check
	./bin/dgx-model sync
