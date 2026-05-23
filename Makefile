TEST_FILES := $(wildcard tests/test_*.py)

.PHONY: lint format format-write typecheck test check $(TEST_FILES)

lint:
	ruff check .

format:
	ruff format --check .

format-write:
	ruff format .

typecheck:
	PYTHONPATH='' pyright --pythonpath $(shell which python)

test: $(TEST_FILES)

$(TEST_FILES):
	PYTHONPATH='' python -m pytest $@ || { code=$$?; [ $$code -eq 5 ] && exit 0 || exit $$code; }

check: lint format typecheck
