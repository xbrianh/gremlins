from ..stages.registry import register_client_factory
from .claude import SubprocessClaudeClient

register_client_factory("claude", lambda _: SubprocessClaudeClient())
