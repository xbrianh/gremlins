.PHONY: lint format typecheck test check

lint:
	ruff check .

format:
	ruff format --check .

typecheck:
	pyright

test:
	pytest

check: lint format typecheck
