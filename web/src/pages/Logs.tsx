import { useEffect, useState, useCallback } from 'react';
import {
  Box,
  Typography,
  Button,
  Table,
  TableBody,
  TableCell,
  TableContainer,
  TableHead,
  TableRow,
  Paper,
  Chip,
  Dialog,
  DialogTitle,
  DialogContent,
  DialogActions,
  Tabs,
  Tab,
  CircularProgress,
  Alert,
  Tooltip,
} from '@mui/material';
import RefreshIcon from '@mui/icons-material/Refresh';
import { api, type LlmLogMeta } from '../api';

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${(ms / 60_000).toFixed(1)}m`;
}

function statusColor(status: string): 'success' | 'error' | 'default' {
  return status === 'succeeded' ? 'success' : status === 'failed' ? 'error' : 'default';
}

function kindLabel(kind: string): string {
  return kind === 'chat_completion_stream' ? 'Stream' : 'Chat';
}

function formatDate(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleString();
  } catch {
    return iso;
  }
}

function textValue(value: unknown): string {
  return typeof value === 'string' ? value : value == null ? '' : String(value);
}

function numberValue(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

function DetailDialog({
  logId,
  open,
  onClose,
}: {
  logId: string | null;
  open: boolean;
  onClose: () => void;
}) {
  const [detail, setDetail] = useState<Record<string, unknown> | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState(0);

  useEffect(() => {
    if (!logId) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setDetail(null);
    api
      .getLlmLog(logId)
      .then((data) => {
        if (!cancelled) {
          setDetail(data);
          setLoading(false);
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setError(String(e));
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [logId]);

  const jsonStr = detail ? JSON.stringify(detail, null, 2) : '';
  const requestText = detail ? JSON.stringify(detail.request ?? null, null, 2) : '';
  const responseText = detail
    ? detail.response !== undefined && detail.response !== null
      ? JSON.stringify(detail.response, null, 2)
      : detail.error
        ? `Error: ${textValue(detail.error)}`
        : '(no response)'
    : '';

  return (
    <Dialog open={open} onClose={onClose} maxWidth="lg" fullWidth>
      <DialogTitle>
        LLM Call Detail
        {detail && (
          <Box component="span" sx={{ ml: 2 }}>
            <Chip
              label={(detail.status as string) ?? '?'}
              color={statusColor((detail.status as string) ?? '')}
              size="small"
            />
            <Chip
              label={kindLabel((detail.kind as string) ?? '')}
              size="small"
              sx={{ ml: 1 }}
            />
          </Box>
        )}
      </DialogTitle>

      {loading && (
        <DialogContent>
          <Box sx={{ display: 'flex', justifyContent: 'center', py: 4 }}>
            <CircularProgress />
          </Box>
        </DialogContent>
      )}

      {error && (
        <DialogContent>
          <Alert severity="error">{error}</Alert>
        </DialogContent>
      )}

      {detail && !loading && (
        <>
          <Box sx={{ borderBottom: 1, borderColor: 'divider', px: 3 }}>
            <Tabs value={tab} onChange={(_, v) => setTab(v)}>
              <Tab label="Summary" />
              <Tab label="Request" />
              <Tab label="Response" />
              <Tab label="Raw JSON" />
            </Tabs>
          </Box>

          <DialogContent>
            {tab === 0 && (
              <Box sx={{ fontFamily: 'monospace', fontSize: '0.85rem', whiteSpace: 'pre-wrap' }}>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>ID:</strong> {textValue(detail.id)}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Model:</strong> {textValue(detail.model)}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Base URL:</strong> {textValue(detail.base_url)}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Created:</strong> {formatDate(textValue(detail.created_at))}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Finished:</strong> {formatDate(textValue(detail.finished_at))}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Duration:</strong> {formatDuration(numberValue(detail.duration_ms))}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Temperature:</strong> {textValue(detail.temperature)}
                </Typography>
                <Typography variant="body2" sx={{ mb: 1 }}>
                  <strong>Max Tokens:</strong> {textValue(detail.max_tokens)}
                </Typography>
                {Boolean(detail.response_format) && (
                  <Typography variant="body2" sx={{ mb: 1 }}>
                    <strong>Response Format:</strong> {textValue(detail.response_format)}
                  </Typography>
                )}
                {Boolean(detail.error) && (
                  <Alert severity="error" sx={{ mt: 2, fontFamily: 'monospace' }}>
                    {textValue(detail.error)}
                  </Alert>
                )}
              </Box>
            )}

            {tab === 1 && (
              <Box
                component="pre"
                sx={{
                  fontFamily: 'monospace',
                  fontSize: '0.8rem',
                  whiteSpace: 'pre-wrap',
                  wordBreak: 'break-word',
                  bgcolor: 'grey.100',
                  p: 2,
                  borderRadius: 1,
                  maxHeight: '60vh',
                  overflow: 'auto',
                }}
              >
                {requestText}
              </Box>
            )}

            {tab === 2 && (
              <Box
                component="pre"
                sx={{
                  fontFamily: 'monospace',
                  fontSize: '0.8rem',
                  whiteSpace: 'pre-wrap',
                  wordBreak: 'break-word',
                  bgcolor: 'grey.100',
                  p: 2,
                  borderRadius: 1,
                  maxHeight: '60vh',
                  overflow: 'auto',
                }}
              >
                {responseText}
              </Box>
            )}

            {tab === 3 && (
              <Box
                component="pre"
                sx={{
                  fontFamily: 'monospace',
                  fontSize: '0.8rem',
                  whiteSpace: 'pre-wrap',
                  wordBreak: 'break-word',
                  bgcolor: 'grey.100',
                  p: 2,
                  borderRadius: 1,
                  maxHeight: '60vh',
                  overflow: 'auto',
                }}
              >
                {jsonStr}
              </Box>
            )}
          </DialogContent>
        </>
      )}

      <DialogActions>
        <Button onClick={onClose}>Close</Button>
      </DialogActions>
    </Dialog>
  );
}

export default function Logs() {
  const [logs, setLogs] = useState<LlmLogMeta[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const loadLogs = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await api.getLlmLogs(100);
      setLogs(res.logs);
      if (res.error) setError(res.error);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadLogs();
  }, [loadLogs]);

  return (
    <Box>
      <Box sx={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', mb: 2 }}>
        <Typography variant="h6" sx={{ fontWeight: 600 }}>
          LLM Call Logs
        </Typography>
        <Button
          variant="outlined"
          size="small"
          startIcon={<RefreshIcon />}
          onClick={loadLogs}
          disabled={loading}
        >
          {loading ? 'Loading...' : 'Refresh'}
        </Button>
      </Box>

      {error && (
        <Alert severity="warning" sx={{ mb: 2 }}>
          {error}
        </Alert>
      )}

      {logs.length === 0 && !loading ? (
        <Typography color="text.secondary">No LLM call logs yet.</Typography>
      ) : (
        <TableContainer component={Paper} variant="outlined">
          <Table size="small">
            <TableHead>
              <TableRow>
                <TableCell>Time</TableCell>
                <TableCell>Model</TableCell>
                <TableCell>Kind</TableCell>
                <TableCell>Status</TableCell>
                <TableCell>Duration</TableCell>
                <TableCell>Temperature</TableCell>
                <TableCell>Max Tokens</TableCell>
                <TableCell>Preview</TableCell>
              </TableRow>
            </TableHead>
            <TableBody>
              {logs.map((log) => (
                <TableRow
                  key={log.id}
                  hover
                  sx={{ cursor: 'pointer' }}
                  onClick={() => setSelectedId(log.id)}
                >
                  <TableCell sx={{ whiteSpace: 'nowrap' }}>
                    <Tooltip title={formatDate(log.created_at)}>
                      <Typography variant="body2">
                        {log.created_at ? formatDate(log.created_at) : '-'}
                      </Typography>
                    </Tooltip>
                  </TableCell>
                  <TableCell>
                    <Typography variant="body2" fontFamily="monospace">
                      {log.model}
                    </Typography>
                  </TableCell>
                  <TableCell>
                    <Chip label={kindLabel(log.kind)} size="small" variant="outlined" />
                  </TableCell>
                  <TableCell>
                    <Chip
                      label={log.status}
                      color={statusColor(log.status)}
                      size="small"
                    />
                  </TableCell>
                  <TableCell>{formatDuration(log.duration_ms)}</TableCell>
                  <TableCell>{log.temperature}</TableCell>
                  <TableCell>{log.max_tokens}</TableCell>
                  <TableCell>
                    <Tooltip title={log.preview ?? ''}>
                      <Typography
                        variant="body2"
                        sx={{
                          maxWidth: 300,
                          overflow: 'hidden',
                          textOverflow: 'ellipsis',
                          whiteSpace: 'nowrap',
                        }}
                      >
                        {log.preview ?? (log.error ? `Error: ${log.error}` : '-')}
                      </Typography>
                    </Tooltip>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </TableContainer>
      )}

      <DetailDialog
        logId={selectedId}
        open={selectedId !== null}
        onClose={() => setSelectedId(null)}
      />
    </Box>
  );
}
