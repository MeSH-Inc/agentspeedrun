#!/usr/bin/env python3
"""Regression tests for the live harness's transcript acceptance rules."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('live_host', Path(__file__).with_name('live-host.py'))
host = importlib.util.module_from_spec(spec)
spec.loader.exec_module(host)


def lines(events):
    return '\n'.join(map(json.dumps, events))


class TranscriptTests(unittest.TestCase):
    def test_claude_requires_both_successful_matched_results_and_final_success(self):
        calls = [{'type': 'tool_use', 'name': 'mcp__plugin__' + name, 'id': name}
                 for name in host.TOOLS]
        results = [{'type': 'tool_result', 'tool_use_id': name} for name in host.TOOLS]
        events = [{'type': 'assistant', 'message': {'content': calls}},
                  {'type': 'user', 'message': {'content': results}},
                  {'type': 'result', 'subtype': 'success', 'is_error': False}]
        self.assertTrue(host.validate_transcript('claude', lines(events)))
        results[0]['is_error'] = True
        self.assertFalse(host.validate_transcript('claude', lines(events)))
        results[0]['is_error'] = False
        events[-1]['subtype'] = 'error'
        self.assertFalse(host.validate_transcript('claude', lines(events)))

    def test_pi_exit_zero_and_provider_error_are_not_success(self):
        self.assertFalse(host.validate_transcript('pi', lines([{'type': 'agent_end',
            'messages': [{'role': 'assistant', 'stopReason': 'error'}]}])))
        events = []
        for name in host.TOOLS:
            events += [{'type': 'tool_execution_start', 'toolName': name, 'toolCallId': name},
                       {'type': 'tool_execution_end', 'toolCallId': name, 'isError': False}]
        events.append({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop'}})
        self.assertTrue(host.validate_transcript('pi', lines(events)))
        events[1]['isError'] = True
        self.assertFalse(host.validate_transcript('pi', lines(events)))

    def test_copilot_does_not_accept_words_without_structured_results(self):
        self.assertFalse(host.validate_transcript('copilot', lines([{'type': 'result',
            'text': 'speedrun_status tool result; speedrun_propose_milestones tool complete'}])))
        events = []
        for name in host.TOOLS:
            events += [{'type': 'tool.execution_start', 'data': {'mcpServerName': 'agentspeedrun',
                        'mcpToolName': name, 'toolCallId': name}},
                       {'type': 'tool.execution_complete', 'data': {'toolCallId': name, 'success': True}}]
        events.append({'type': 'assistant.idle'})
        self.assertTrue(host.validate_transcript('copilot', lines(events)))
        events[1]['data']['success'] = False
        self.assertFalse(host.validate_transcript('copilot', lines(events)))

    def test_opencode_requires_both_completed_tools_and_finished_model(self):
        events = [{'type': 'tool_use', 'part': {'tool': name, 'state': {'status': 'completed'}}}
                  for name in host.TOOLS]
        events.append({'type': 'step_finish', 'part': {'reason': 'stop'}})
        self.assertTrue(host.validate_transcript('opencode', lines(events)))
        events[0]['part']['state']['status'] = 'error'
        self.assertFalse(host.validate_transcript('opencode', lines(events)))

    def test_opencode_execute_requires_actual_forwarded_tool_results(self):
        events = [{'type': 'tool_use', 'part': {'tool': 'execute', 'state': {'status': 'completed'}}},
                  {'type': 'step_finish', 'part': {'reason': 'stop'}}]
        self.assertFalse(host.validate_transcript('opencode', lines(events)))
        self.assertFalse(host.validate_transcript('opencode', lines(events), 'speedrun_status\n'))
        self.assertTrue(host.validate_transcript('opencode', lines(events), '\n'.join(host.TOOLS)))


if __name__ == '__main__':
    unittest.main()
