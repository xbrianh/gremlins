from .claude import SubprocessClaudeClient
from ..stages.registry import register_client_factory

register_client_factory("claude", lambda _: SubprocessClaudeClient())
