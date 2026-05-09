TEST_FILES := $(wildcard tests/test_*.py)

.PHONY: lint format typecheck test check $(TEST_FILES)

lint:
	ruff check .

format:
	ruff format --check .

typecheck:
	pyright

test: $(TEST_FILES)

$(TEST_FILES):
	pytest $@ || { code=$$?; [ $$code -eq 5 ] && exit 0 || exit $$code; }

check: lint format typecheck
