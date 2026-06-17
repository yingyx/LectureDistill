import { useEffect, useState, useCallback } from 'react';
import {
  api,
  type ProcessRecord,
  type ProcessOutputContent,
  type SourceRecord,
} from '../api';
import {
  Button,
  Dialog,
  DialogTitle,
  DialogContent,
  DialogActions,
  TextField,
  Chip,
  Alert,
  AlertTitle,
  Stack,
  Card,
  CardContent,
  CardActions,
  Typography,
  IconButton,
  Tooltip,
  Box,
  CircularProgress,
  FormControl,
  InputLabel,
  Select,
  MenuItem,
  List,
  ListItemButton,
  ListItemText,
  ListItemIcon,
  Tabs,
  Tab,
  Paper,
  LinearProgress,
  FormControlLabel,
  Checkbox,
} from '@mui/material';
import AddIcon from '@mui/icons-material/Add';
import DeleteIcon from '@mui/icons-material/Delete';
import RefreshIcon from '@mui/icons-material/Refresh';
import EditIcon from '@mui/icons-material/Edit';
import ReactMarkdown from 'react-markdown';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function fmtDateTime(iso: string): string {
  if (iso.length >= 16) return iso.slice(0, 16).replace('T', ' ');
  return iso;
}

function statusColor(
  status: string,
): 'success' | 'warning' | 'error' | 'default' {
  switch (status) {
    case 'ready':
      return 'success';
    case 'processing':
      return 'warning';
    case 'failed':
      return 'error';
    default:
      return 'default';
  }
}

function statusLabel(status: string): string {
  switch (status) {
    case 'ready':
      return 'Ready';
    case 'processing':
      return 'Processing';
    case 'failed':
      return 'Failed';
    default:
      return status;
  }
}

function numberMeta(meta: Record<string, unknown> | undefined, key: string): number | null {
  const value = meta?.[key];
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string') {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

function outputProgress(output?: { metadata?: Record<string, unknown>; status?: string }) {
  if (!output) return null;
  const current = numberMeta(output.metadata, 'progress_current');
  const total = numberMeta(output.metadata, 'progress_total');
  const labelValue = output.metadata?.['progress_label'];
  const label = typeof labelValue === 'string' ? labelValue : '';
  if (current == null || total == null || total <= 0) return null;
  const value = Math.max(0, Math.min(100, Math.round((current / total) * 100)));
  return { current, total, value, label };
}

function DiffView({ diff }: { diff: string }) {
  const lines = diff ? diff.split('\n') : ['(no diff)'];
  return (
    <Paper
      variant="outlined"
      sx={{
        maxHeight: 520,
        overflow: 'auto',
        fontFamily: 'Consolas, Courier New, monospace',
        fontSize: '0.8rem',
        bgcolor: 'grey.50',
      }}
    >
      {lines.map((line, idx) => {
        const isAdd = line.startsWith('+') && !line.startsWith('+++');
        const isRemove = line.startsWith('-') && !line.startsWith('---');
        const isHunk = line.startsWith('@@');
        return (
          <Box
            key={idx}
            component="div"
            sx={{
              whiteSpace: 'pre-wrap',
              px: 1.5,
              py: 0.15,
              bgcolor: isAdd
                ? '#e8f5e9'
                : isRemove
                  ? '#ffebee'
                  : isHunk
                    ? '#e3f2fd'
                    : 'transparent',
              color: isAdd
                ? 'success.dark'
                : isRemove
                  ? 'error.dark'
                  : isHunk
                    ? 'info.dark'
                    : 'text.primary',
            }}
          >
            {line || ' '}
          </Box>
        );
      })}
    </Paper>
  );
}

// ---------------------------------------------------------------------------
// AddProcessDialog
// ---------------------------------------------------------------------------

interface AddProcessDialogProps {
  onClose: () => void;
  onCreated: () => void;
}

function AddProcessDialog({ onClose, onCreated }: AddProcessDialogProps) {
  const [sources, setSources] = useState<SourceRecord[]>([]);
  const [sourcesLoading, setSourcesLoading] = useState(true);
  const [selectedSourceIds, setSelectedSourceIds] = useState<string[]>([]);
  const [selectedOutputKinds, setSelectedOutputKinds] = useState<string[]>(['note_patch']);
  const [maxPages, setMaxPages] = useState(2);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .getSources()
      .then((res) => setSources(res.sources))
      .catch((e) => setError('Failed to load sources: ' + String(e)))
      .finally(() => setSourcesLoading(false));
  }, []);

  const noteSourceCount = selectedSourceIds.filter((id) => {
    const src = sources.find((s) => s.id === id);
    return src?.kind === 'note';
  }).length;
  const courseSourceCount = selectedSourceIds.filter((id) => {
    const src = sources.find((s) => s.id === id);
    return src?.kind === 'transcript_course';
  }).length;
  const daySourceCount = selectedSourceIds.filter((id) => {
    const src = sources.find((s) => s.id === id);
    return src?.kind === 'transcript_day';
  }).length;

  const handleToggleSource = (id: string) => {
    setSelectedSourceIds((prev) =>
      prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id],
    );
  };

  const handleToggleOutput = (kind: string) => {
    setSelectedOutputKinds((prev) => {
      const selected = prev.includes(kind);
      if (kind === 'note_patch') {
        return selected ? prev.filter((x) => x !== kind) : [...prev, kind];
      }
      if (kind === 'reference_digest') {
        if (selected && prev.includes('cheating_sheet')) return prev;
        return selected ? prev.filter((x) => x !== kind) : [...prev, kind];
      }
      if (kind === 'cheating_sheet') {
        if (selected) return prev.filter((x) => x !== kind);
        return Array.from(new Set([...prev, 'reference_digest', 'cheating_sheet']));
      }
      return prev;
    });
  };

  const handleCreate = async () => {
    if (selectedSourceIds.length === 0) {
      setError('Please select at least one source.');
      return;
    }
    if (selectedOutputKinds.length === 0) {
      setError('Please select at least one output method.');
      return;
    }
    if (noteSourceCount > 1) {
      setError('At most one Note source is allowed.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await api.createProcess({
        source_ids: selectedSourceIds,
        outputs: selectedOutputKinds.map((kind) => ({
          kind,
          max_pages: kind === 'cheating_sheet' ? maxPages : undefined,
        })),
      });
      if (res.status === 'failed' || res.error) {
        setError(res.error || 'Failed to create process.');
      } else {
        onCreated();
        onClose();
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <Dialog open onClose={onClose} maxWidth="sm" fullWidth>
      <DialogTitle>Add Processing</DialogTitle>
      <DialogContent>
        <Stack spacing={2} sx={{ mt: 1 }}>
          {error && <Alert severity="error">{error}</Alert>}

          <Typography variant="subtitle2" color="text.secondary">
            Select Sources (at most one Note source)
          </Typography>
          {sourcesLoading ? (
            <CircularProgress size={24} />
          ) : sources.length === 0 ? (
            <Alert severity="info">
              No Sources available. Create Sources first.
            </Alert>
          ) : (
            <Box sx={{ maxHeight: 200, overflow: 'auto', border: 1, borderColor: 'divider', borderRadius: 1 }}>
              <List dense>
                {sources.map((src) => {
                  const isNote = src.kind === 'note';
                  const isCourse = src.kind === 'transcript_course';
                  const isSelected = selectedSourceIds.includes(src.id);
                  const wouldExceedNoteLimit =
                    isNote && noteSourceCount >= 1 && !isSelected;
                  return (
                    <ListItemButton
                      key={src.id}
                      selected={isSelected}
                      disabled={wouldExceedNoteLimit}
                      onClick={() => handleToggleSource(src.id)}
                    >
                      <ListItemIcon sx={{ minWidth: 36 }}>
                        <Chip
                          label={isNote ? 'Note' : isCourse ? 'Course' : 'Transcript'}
                          size="small"
                          color={isNote ? 'primary' : 'default'}
                        />
                      </ListItemIcon>
                      <ListItemText
                        primary={src.title}
                        secondary={`${src.length || ''}  ${fmtDateTime(src.created_at)}`}
                      />
                    </ListItemButton>
                  );
                })}
              </List>
            </Box>
          )}

          <Typography variant="body2" color="text.secondary">
            Selected: {selectedSourceIds.length} source(s) ({noteSourceCount} note,{' '}
            {courseSourceCount} course transcript, {daySourceCount} transcript day)
          </Typography>
          {courseSourceCount > 0 && (
            <Alert severity="info">
              BM25 will select relevant lecture dates for each note section.
            </Alert>
          )}

          <Box>
            <Typography variant="subtitle2" color="text.secondary" sx={{ mb: 0.5 }}>
              Output Methods
            </Typography>
            <Stack spacing={0.5}>
              <FormControlLabel
                control={
                  <Checkbox
                    checked={selectedOutputKinds.includes('note_patch')}
                    onChange={() => handleToggleOutput('note_patch')}
                  />
                }
                label="Note Patch"
              />
              <FormControlLabel
                control={
                  <Checkbox
                    checked={selectedOutputKinds.includes('reference_digest')}
                    disabled={selectedOutputKinds.includes('cheating_sheet')}
                    onChange={() => handleToggleOutput('reference_digest')}
                  />
                }
                label="Reference Digest"
              />
              <FormControlLabel
                control={
                  <Checkbox
                    checked={selectedOutputKinds.includes('cheating_sheet')}
                    onChange={() => handleToggleOutput('cheating_sheet')}
                  />
                }
                label="Cheating Sheet"
              />
            </Stack>
            {selectedOutputKinds.includes('cheating_sheet') && (
              <TextField
                sx={{ mt: 1 }}
                size="small"
                type="number"
                label="Max Pages"
                value={maxPages}
                inputProps={{ min: 1, max: 20 }}
                onChange={(e) =>
                  setMaxPages(Math.max(1, Math.min(20, Number(e.target.value) || 2)))
                }
                fullWidth
              />
            )}
            {selectedOutputKinds.includes('cheating_sheet') && (
              <Alert severity="info" sx={{ mt: 1 }}>
                Cheating Sheet depends on Reference Digest, so Reference Digest is included automatically.
              </Alert>
            )}
          </Box>
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button onClick={onClose}>Cancel</Button>
        <Button
          variant="contained"
          onClick={handleCreate}
          disabled={loading || selectedSourceIds.length === 0}
        >
          {loading ? <CircularProgress size={20} /> : 'Create'}
        </Button>
      </DialogActions>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// ProcessDetail
// ---------------------------------------------------------------------------

interface ProcessDetailProps {
  process: ProcessRecord;
  onRefresh: () => void;
  onClose: () => void;
}

function ProcessDetail({ process, onRefresh, onClose }: ProcessDetailProps) {
  const [selectedOutputId, setSelectedOutputId] = useState<string | null>(
    process.outputs.length > 0 ? process.outputs[0].id : null,
  );
  const [outputContent, setOutputContent] =
    useState<ProcessOutputContent | null>(null);
  const [contentLoading, setContentLoading] = useState(false);
  const [contentError, setContentError] = useState<string | null>(null);
  const [tabIndex, setTabIndex] = useState(0);
  const [instruction, setInstruction] = useState('');
  const [revising, setRevising] = useState(false);
  const [reviseError, setReviseError] = useState<string | null>(null);
  const [addingOutput, setAddingOutput] = useState(false);
  const [addOutputError, setAddOutputError] = useState<string | null>(null);
  const [cheatingSheetMaxPages, setCheatingSheetMaxPages] = useState(2);
  const [streamText, setStreamText] = useState<string | null>(null);

  const selectedOutput = process.outputs.find(
    (o) => o.id === selectedOutputId,
  );

  const loadOutputContent = useCallback(async (outputId: string, quiet = false) => {
    if (!quiet) setContentLoading(true);
    setContentError(null);
    try {
      const content = await api.getProcessOutput(process.id, outputId);
      setOutputContent(content);
      // Default to Diff tab if has_base_note, otherwise Markdown tab.
      if (!quiet) setTabIndex(content.output.kind === 'cheating_sheet' ? 1 : content.has_base_note ? 0 : 1);
    } catch (e) {
      setContentError(String(e));
    } finally {
      if (!quiet) setContentLoading(false);
    }
  }, [process.id]);

  useEffect(() => {
    if (selectedOutputId) {
      loadOutputContent(selectedOutputId);
    } else {
      setOutputContent(null);
    }
  }, [selectedOutputId, loadOutputContent]);

  useEffect(() => {
    if (!selectedOutputId || process.status !== 'processing') return;
    const timer = setInterval(() => {
      onRefresh();
      loadOutputContent(selectedOutputId, true);
    }, 3000);
    return () => clearInterval(timer);
  }, [process.status, selectedOutputId, onRefresh, loadOutputContent]);

  // Auto-connect to SSE streaming when viewing a processing output.
  useEffect(() => {
    if (!selectedOutputId || selectedOutput?.status !== 'processing') {
      setStreamText(null);
      return;
    }
    setStreamText('');
    const url = api.streamProcessOutputUrl(process.id, selectedOutputId);
    const es = new EventSource(url);
    es.addEventListener('chunk', (e: MessageEvent) => {
      setStreamText((prev) => (prev ?? '') + e.data);
    });
    es.addEventListener('done', () => {
      es.close();
      onRefresh();
    });
    es.addEventListener('error', () => es.close());
    return () => es.close();
  }, [process.id, selectedOutputId, selectedOutput?.status, onRefresh]);

  const handleRevise = async () => {
    if (!selectedOutputId || !instruction.trim()) return;
    setRevising(true);
    setReviseError(null);
    try {
      const res = await api.reviseProcessOutput(
        process.id,
        selectedOutputId,
        instruction,
      );
      if (res.status === 'failed' || res.error) {
        setReviseError(res.error || 'Revision failed.');
      } else {
        setOutputContent((prev) =>
          prev
            ? {
                ...prev,
                markdown: res.markdown,
                diff: res.diff,
                has_base_note: res.has_base_note,
              }
            : prev,
        );
        setInstruction('');
        setReviseError(null);
        // Switch to tab showing the result: diff if available, else markdown.
        if (res.has_base_note && res.diff) setTabIndex(0);
        else setTabIndex(1);
        onRefresh();
      }
    } catch (e) {
      setReviseError(String(e));
    } finally {
      setRevising(false);
    }
  };

  const handleDeleteOutput = async () => {
    if (!selectedOutputId) return;
    try {
      await api.deleteProcessOutput(process.id, selectedOutputId);
      onRefresh();
      onClose();
    } catch (e) {
      setContentError('Failed to delete output: ' + String(e));
    }
  };

  const handleRetryOutput = async () => {
    if (!selectedOutputId) return;
    try {
      const res = await api.retryProcessOutput(process.id, selectedOutputId);
      if (res.error) {
        setContentError('Failed to retry output: ' + res.error);
      } else {
        onRefresh();
      }
    } catch (e) {
      setContentError('Failed to retry output: ' + String(e));
    }
  };

  const handleAddOutput = async (kind: 'note_patch' | 'reference_digest' | 'cheating_sheet') => {
    setAddingOutput(true);
    setAddOutputError(null);
    try {
      const res = await api.addProcessOutput(
        process.id,
        kind,
        kind === 'cheating_sheet' ? cheatingSheetMaxPages : undefined,
      );
      if (res.error) {
        setAddOutputError(res.error);
      } else {
        onRefresh();
        setAddOutputError(null);
      }
    } catch (e) {
      setAddOutputError(String(e));
    } finally {
      setAddingOutput(false);
    }
  };

  const hasNotePatch = process.outputs.some(
    (o) => o.kind === 'note_patch',
  );
  const hasReferenceDigest = process.outputs.some(
    (o) => o.kind === 'reference_digest',
  );
  const hasCheatingSheet = process.outputs.some(
    (o) => o.kind === 'cheating_sheet',
  );
  const selectedProgress = outputProgress(selectedOutput);
  const selectedIsCheatingSheet = selectedOutput?.kind === 'cheating_sheet';
  const selectedIsNotePatch = selectedOutput?.kind === 'note_patch';

  return (
    <Dialog open onClose={onClose} maxWidth="lg" fullWidth>
      <DialogTitle>
        {process.title}
        <Chip
          label={statusLabel(process.status)}
          color={statusColor(process.status)}
          size="small"
          sx={{ ml: 2 }}
        />
      </DialogTitle>
      <DialogContent>
        <Stack spacing={2} sx={{ mt: 1 }}>
          {process.last_error && (
            <Alert severity="error">
              <AlertTitle>Error</AlertTitle>
              {process.last_error}
            </Alert>
          )}

          {/* Output selector */}
          {process.outputs.length > 0 && (
            <FormControl fullWidth size="small">
              <InputLabel>Output</InputLabel>
              <Select
                value={selectedOutputId || ''}
                label="Output"
                onChange={(e) => setSelectedOutputId(e.target.value)}
              >
                {process.outputs.map((o) => (
                  <MenuItem key={o.id} value={o.id}>
                    {o.title} ({statusLabel(o.status)})
                  </MenuItem>
                ))}
              </Select>
            </FormControl>
          )}

          {selectedOutput && selectedOutput.status === 'processing' && selectedProgress && (
            <Box>
              <LinearProgress variant="determinate" value={selectedProgress.value} />
              <Typography variant="caption" color="text.secondary">
                {selectedProgress.value}% · {selectedProgress.label || 'processing'}
              </Typography>
            </Box>
          )}

          {/* Content area */}
          {contentLoading ? (
            <Box sx={{ display: 'flex', justifyContent: 'center', py: 4 }}>
              <CircularProgress />
            </Box>
          ) : contentError ? (
            <Alert severity="error">{contentError}</Alert>
          ) : selectedOutput && outputContent ? (
            <>
              <Tabs
                value={tabIndex}
                onChange={(_, v) => setTabIndex(v)}
                sx={{ borderBottom: 1, borderColor: 'divider' }}
              >
                <Tab label="Diff" disabled={!outputContent.has_base_note || selectedIsCheatingSheet} />
                <Tab label={selectedIsCheatingSheet ? 'Source Markdown' : (selectedOutput?.kind === 'reference_digest' ? 'Digest Markdown' : 'Markdown')} />
              </Tabs>

              {/* Diff tab */}
              {tabIndex === 0 && <DiffView diff={outputContent.diff} />}

              {/* Markdown tab */}
              {tabIndex === 1 && (
                <Paper
                  variant="outlined"
                  sx={{
                    p: 2,
                    maxHeight: 400,
                    overflow: 'auto',
                  }}
                >
                  {streamText !== null ? (
                    <Box>
                      <Chip label="Live" color="warning" size="small" sx={{ mb: 1 }} />
                      <Box className="markdown-body">
                        <ReactMarkdown>{streamText || '*Waiting for output...*'}</ReactMarkdown>
                      </Box>
                      <Box component="span" sx={{ animation: 'blink 1s infinite', color: 'warning.main' }}>▌</Box>
                      <style>{`@keyframes blink { 0%,100% { opacity:1; } 50% { opacity:0; } }`}</style>
                    </Box>
                  ) : (
                    <Box className="markdown-body">
                      <ReactMarkdown>{outputContent.markdown || '(empty markdown)'}</ReactMarkdown>
                    </Box>
                  )}
                </Paper>
              )}

              {outputContent.retrieval && outputContent.retrieval.length > 0 && (
                <Paper variant="outlined" sx={{ p: 2 }}>
                  <Typography variant="subtitle2" sx={{ mb: 1 }}>
                    Retrieval
                  </Typography>
                  <Stack spacing={1}>
                    {outputContent.retrieval.map((trace, idx) => (
                      <Box key={idx}>
                        <Typography variant="body2" fontWeight={600}>
                          {trace.section}
                        </Typography>
                        {trace.skipped_reason ? (
                          <Typography variant="caption" color="text.secondary">
                            {trace.skipped_reason}
                          </Typography>
                        ) : (
                          trace.matches.map((m) => (
                            <Typography key={m.date} variant="caption" display="block" color="text.secondary">
                              {m.date} score {m.score.toFixed(2)} - {m.timestamp_ranges.slice(0, 3).map((r) => `${r.video_id} ${Math.round(r.start)}-${Math.round(r.end)}s`).join(', ')}
                            </Typography>
                          ))
                        )}
                      </Box>
                    ))}
                  </Stack>
                </Paper>
              )}

              {selectedOutput.last_error && (
                <Alert severity="error">{selectedOutput.last_error}</Alert>
              )}

              {selectedIsCheatingSheet && outputContent.artifact_path && (
                <Alert severity={selectedOutput.status === 'ready' ? 'success' : 'info'}>
                  PDF: {outputContent.artifact_path}
                  {typeof selectedOutput.metadata?.page_count === 'number'
                    ? ` (${selectedOutput.metadata.page_count} page(s))`
                    : ''}
                </Alert>
              )}

              {/* Revision section - only for Note Patch */}
              {selectedIsNotePatch && (
              <Stack direction="row" spacing={1} alignItems="flex-start">
                <TextField
                  fullWidth
                  size="small"
                  label="Revise with natural language instruction"
                  value={instruction}
                  onChange={(e) => setInstruction(e.target.value)}
                  disabled={revising}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !e.shiftKey) {
                      e.preventDefault();
                      handleRevise();
                    }
                  }}
                />
                <Button
                  variant="contained"
                  onClick={handleRevise}
                  disabled={revising || !instruction.trim()}
                  startIcon={
                    revising ? <CircularProgress size={16} /> : <EditIcon />
                  }
                >
                  Apply
                </Button>
              </Stack>
              )}
              {reviseError && <Alert severity="error">{reviseError}</Alert>}

              {/* Delete output */}
              <Stack direction="row" spacing={1}>
                {selectedOutput.status === 'failed' && (
                  <Button
                    variant="outlined"
                    color="warning"
                    startIcon={<RefreshIcon />}
                    onClick={handleRetryOutput}
                  >
                    Retry Output
                  </Button>
                )}
                <Button
                  variant="outlined"
                  color="error"
                  startIcon={<DeleteIcon />}
                  onClick={handleDeleteOutput}
                >
                  Delete Output
                </Button>
              </Stack>
            </>
          ) : process.outputs.length === 0 ? (
            <Alert severity="info">
              No outputs yet. Add an output method below.
            </Alert>
          ) : null}

          {/* Add output method */}
          <Box
            sx={{ borderTop: 1, borderColor: 'divider', pt: 2, mt: 1 }}
          >
            <Stack direction="row" spacing={1} alignItems="center">
              <Button
                variant="outlined"
                startIcon={
                  addingOutput ? (
                    <CircularProgress size={16} />
                  ) : (
                    <AddIcon />
                  )
                }
                onClick={() => handleAddOutput('note_patch')}
                disabled={addingOutput || hasNotePatch}
              >
                {hasNotePatch
                  ? 'Note Patch already exists'
                  : 'Add Note Patch'}
              </Button>
              {hasNotePatch && (
                <Typography variant="caption" color="text.secondary">
                  Only one Note Patch output is supported.
                </Typography>
              )}
            </Stack>
            <Stack direction="row" spacing={1} alignItems="center" sx={{ mt: 1 }}>
              <Button
                variant="outlined"
                startIcon={
                  addingOutput ? (
                    <CircularProgress size={16} />
                  ) : (
                    <AddIcon />
                  )
                }
                onClick={() => handleAddOutput('reference_digest')}
                disabled={addingOutput || hasReferenceDigest}
              >
                {hasReferenceDigest
                  ? 'Reference Digest already exists'
                  : 'Add Reference Digest'}
              </Button>
              {hasReferenceDigest && (
                <Typography variant="caption" color="text.secondary">
                  Only one Reference Digest output is supported.
                </Typography>
              )}
            </Stack>
            <Stack direction="row" spacing={1} alignItems="center" sx={{ mt: 1 }}>
              <TextField
                size="small"
                type="number"
                label="Max Pages"
                value={cheatingSheetMaxPages}
                inputProps={{ min: 1, max: 20 }}
                onChange={(e) =>
                  setCheatingSheetMaxPages(Math.max(1, Math.min(20, Number(e.target.value) || 2)))
                }
                sx={{ width: 120 }}
              />
              <Button
                variant="outlined"
                startIcon={
                  addingOutput ? (
                    <CircularProgress size={16} />
                  ) : (
                    <AddIcon />
                  )
                }
                onClick={() => handleAddOutput('cheating_sheet')}
                disabled={addingOutput || hasCheatingSheet}
              >
                {hasCheatingSheet ? 'Cheating Sheet already exists' : 'Add Cheating Sheet'}
              </Button>
              <Typography variant="caption" color="text.secondary">
                Depends on Reference Digest.
              </Typography>
            </Stack>
            {addOutputError && (
              <Alert severity="error" sx={{ mt: 1 }}>
                {addOutputError}
              </Alert>
            )}
          </Box>
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button onClick={onClose}>Close</Button>
      </DialogActions>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// Processing page
// ---------------------------------------------------------------------------

export default function Processing() {
  const [processes, setProcesses] = useState<ProcessRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showAddDialog, setShowAddDialog] = useState(false);
  const [selectedProcess, setSelectedProcess] =
    useState<ProcessRecord | null>(null);

  const loadProcesses = useCallback(async () => {
    setError(null);
    try {
      const res = await api.getProcesses();
      setProcesses(res.processes);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadProcesses();
  }, [loadProcesses]);

  useEffect(() => {
    if (!selectedProcess) return;
    const latest = processes.find((p) => p.id === selectedProcess.id);
    if (latest && latest !== selectedProcess) {
      setSelectedProcess(latest);
    }
  }, [processes, selectedProcess]);

  // Auto-poll when any process is "processing".
  useEffect(() => {
    const hasProcessing = processes.some(
      (p) => p.status === 'processing',
    );
    if (!hasProcessing) return;

    const timer = setInterval(() => {
      loadProcesses();
    }, 3000);

    return () => clearInterval(timer);
  }, [processes, loadProcesses]);

  const handleRefresh = useCallback(() => {
    setLoading(true);
    loadProcesses();
  }, [loadProcesses]);

  const handleRetryProcess = useCallback(
    async (processId: string, force: boolean) => {
      setError(null);
      try {
        const res = await api.retryProcess(processId, force);
        if (res.error) {
          setError('Retry failed: ' + res.error);
        } else {
          loadProcesses();
        }
      } catch (e) {
        setError('Retry failed: ' + String(e));
      }
    },
    [loadProcesses],
  );

  return (
    <Box>
      {/* Top bar */}
      <Stack
        direction="row"
        justifyContent="space-between"
        alignItems="center"
        sx={{ mb: 3 }}
      >
        <Typography variant="h5">Processing</Typography>
        <Stack direction="row" spacing={1}>
          <Tooltip title="Refresh">
            <IconButton onClick={handleRefresh} disabled={loading}>
              {loading ? <CircularProgress size={20} /> : <RefreshIcon />}
            </IconButton>
          </Tooltip>
          <Button
            variant="contained"
            startIcon={<AddIcon />}
            onClick={() => setShowAddDialog(true)}
          >
            Add Processing
          </Button>
        </Stack>
      </Stack>

      {error && (
        <Alert severity="error" sx={{ mb: 2 }}>
          {error}
        </Alert>
      )}

      {/* Process list */}
      {loading && processes.length === 0 ? (
        <Box sx={{ display: 'flex', justifyContent: 'center', py: 4 }}>
          <CircularProgress />
        </Box>
      ) : processes.length === 0 ? (
        <Alert severity="info">
          No processing records yet. Add a Processing to get started.
        </Alert>
      ) : (
        <Stack spacing={2}>
          {processes.map((proc) => (
            <Card
              key={proc.id}
              variant="outlined"
              sx={{ cursor: 'pointer' }}
              onClick={() => setSelectedProcess(proc)}
            >
              <CardContent sx={{ pb: 0 }}>
                <Stack
                  direction="row"
                  justifyContent="space-between"
                  alignItems="center"
                >
                  <Typography variant="h6" sx={{ fontSize: '1rem' }}>
                    {proc.title}
                  </Typography>
                  <Chip
                    label={statusLabel(proc.status)}
                    color={statusColor(proc.status)}
                    size="small"
                  />
                </Stack>
                <Stack
                  direction="row"
                  spacing={2}
                  sx={{ mt: 0.5, color: 'text.secondary', fontSize: '0.8rem' }}
                >
                  <span>{fmtDateTime(proc.created_at)}</span>
                  <span>{proc.source_ids.length} source(s)</span>
                  <span>{proc.outputs.length} output(s)</span>
                  {proc.outputs.length > 0 && (
                    <span>
                      Outputs:{' '}
                      {proc.outputs.map((o) => o.kind).join(', ')}
                    </span>
                  )}
                </Stack>
                {proc.last_error && (
                  <Alert severity="error" sx={{ mt: 1, py: 0 }}>
                    {proc.last_error}
                  </Alert>
                )}
                {proc.status === 'processing' && (() => {
                  const activeOutput = proc.outputs.find((o) => o.status === 'processing') || proc.outputs[0];
                  const progress = outputProgress(activeOutput);
                  if (!progress) return null;
                  return (
                    <Box sx={{ mt: 1 }}>
                      <LinearProgress variant="determinate" value={progress.value} />
                      <Typography variant="caption" color="text.secondary">
                        {progress.value}% · {progress.label || activeOutput.title}
                      </Typography>
                    </Box>
                  );
                })()}
              </CardContent>
              <CardActions>
                <Button
                  size="small"
                  onClick={(e) => {
                    e.stopPropagation();
                    setSelectedProcess(proc);
                  }}
                >
                  View Details
                </Button>
                {proc.outputs.some((o) => o.status === 'failed') && (
                  <Button
                    size="small"
                    color="warning"
                    onClick={(e) => {
                      e.stopPropagation();
                      handleRetryProcess(proc.id, false);
                    }}
                  >
                    Retry Failed
                  </Button>
                )}
                {proc.status !== 'processing' && (
                  <Button
                    size="small"
                    color="error"
                    onClick={(e) => {
                      e.stopPropagation();
                      handleRetryProcess(proc.id, true);
                    }}
                  >
                    Force Retry All
                  </Button>
                )}
              </CardActions>
            </Card>
          ))}
        </Stack>
      )}

      {/* Add dialog */}
      {showAddDialog && (
        <AddProcessDialog
          onClose={() => setShowAddDialog(false)}
          onCreated={loadProcesses}
        />
      )}

      {/* Detail dialog */}
      {selectedProcess && (
        <ProcessDetail
          process={selectedProcess}
          onRefresh={loadProcesses}
          onClose={() => setSelectedProcess(null)}
        />
      )}
    </Box>
  );
}
