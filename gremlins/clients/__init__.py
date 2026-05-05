from ..stages.registry import register_client_factory
from .claude import SubprocessClaudeClient
from .copilot import SubprocessCopilotClient

register_client_factory("claude", lambda _: SubprocessClaudeClient())
register_client_factory("copilot", lambda _: SubprocessCopilotClient())
