TEST_FILES := $(wildcard tests/test_*.py)

.PHONY: lint format typecheck test check $(TEST_FILES)

lint:
	ruff check .

format:
	ruff format --check .

typecheck:
	PYTHONPATH='' pyright

test: $(TEST_FILES)

$(TEST_FILES):
	PYTHONPATH='' pytest $@ || { code=$$?; [ $$code -eq 5 ] && exit 0 || exit $$code; }

check: lint format typecheck
