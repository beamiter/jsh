"""Durable attempt admission before evidence-directory allocation."""
from pathlib import Path
import unittest
from unittest.mock import patch

from test_research_auto import AutoFixture, r


class AttemptAllocationTests(AutoFixture):
    def test_failed_reservation_write_does_not_strand_next_directory(self):
        self.ready()
        before = self.loop.state_path.read_bytes()
        directory = self.loop.directory / 'attempt-000001'
        with patch.object(self.loop, 'save', side_effect=OSError('reservation write failed')), \
             self.assertRaises(OSError):
            self.loop.run(execute=True)
        self.assertEqual(self.loop.state_path.read_bytes(), before)
        self.assertFalse(directory.exists())
        self.assertEqual(self.server.calls, [])

        with patch.object(self.loop, 'native', side_effect=r.ResearchError('fixture ends preparation')):
            result = self.loop.run(execute=True)
        self.assertEqual(len(result['attempts']), 1)
        self.assertEqual(result['attempts'][0]['failure_code'], 'preparation_failed')
        self.assertTrue(directory.is_dir())
        self.assertEqual(self.server.calls, [])

    def test_directory_failure_is_recorded_and_next_attempt_can_proceed(self):
        self.ready()
        first = self.loop.directory / 'attempt-000001'
        mkdir = Path.mkdir

        def fail_first(directory, *args, **kwargs):
            if directory == first:
                state = self.loop.load()
                self.assertEqual(len(state['attempts']), 1)
                self.assertEqual(state['attempts'][0]['status'], 'running')
                self.assertEqual(state['reserved_output_tokens'], 256)
                raise OSError('evidence allocation failed')
            return mkdir(directory, *args, **kwargs)

        with patch.object(Path, 'mkdir', new=fail_first), \
             patch.object(self.loop, 'native', side_effect=r.ResearchError('fixture ends preparation')) as native:
            result = self.loop.run(limit=2, execute=True)
        self.assertEqual(len(result['attempts']), 2)
        self.assertEqual([attempt['failure_code'] for attempt in result['attempts']],
                         ['preparation_failed', 'preparation_failed'])
        self.assertEqual(result['reserved_output_tokens'], 512)
        self.assertEqual(native.call_count, 1)
        self.assertFalse(first.exists())
        self.assertTrue((self.loop.directory / 'attempt-000002').is_dir())
        self.assertEqual(self.server.calls, [])

    def test_crash_after_reservation_recovers_without_allocating_or_replaying(self):
        self.ready()
        save = self.loop.save

        class SimulatedCrash(BaseException):
            pass

        def persisted_then_crashed(state):
            save(state)
            raise SimulatedCrash

        with patch.object(self.loop, 'save', side_effect=persisted_then_crashed), \
             self.assertRaises(SimulatedCrash):
            self.loop.run(execute=True)
        interrupted = self.loop.load()
        self.assertEqual(interrupted['attempts'][0]['status'], 'running')
        self.assertFalse((self.loop.directory / 'attempt-000001').exists())
        recovered = self.loop.recover()
        self.assertEqual(recovered['attempts'][0]['status'], 'interrupted')
        self.assertEqual(recovered['charged_seconds'], interrupted['charged_seconds'])
        self.assertEqual(recovered['reserved_output_tokens'], 256)
        with patch.object(self.loop, 'native', side_effect=r.ResearchError('fixture ends preparation')):
            result = self.loop.run(execute=True)
        self.assertEqual(len(result['attempts']), 2)
        self.assertEqual(result['attempts'][0]['charged_seconds'], interrupted['charged_seconds'])
        self.assertTrue((self.loop.directory / 'attempt-000002').is_dir())
        self.assertEqual(self.server.calls, [])


if __name__ == '__main__':
    unittest.main()
