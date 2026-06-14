import { Fragment, useEffect, useState, useCallback, useRef } from 'react';
import {
  api,
  type SourceRecord,
  type CanvasCourse,
  type CourseDate,
  type CourseDateIndex,
  type SourceActionResponse,
} from '../api';
import {
  Button,
  Dialog,
  DialogTitle,
  DialogContent,
  DialogActions,
  TextField,
  Select,
  MenuItem,
  FormControl,
  InputLabel,
  Chip,
  LinearProgress,
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
  FormHelperText,
  Collapse,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableRow,
} from '@mui/material';
import AddIcon from '@mui/icons-material/Add';
import DeleteIcon from '@mui/icons-material/Delete';
import SyncIcon from '@mui/icons-material/Sync';
import UploadIcon from '@mui/icons-material/Upload';
import QuestionAnswerIcon from '@mui/icons-material/QuestionAnswer';
import ExpandMoreIcon from '@mui/icons-material/ExpandMore';
import RefreshIcon from '@mui/icons-material/Refresh';
import ReactMarkdown from 'react-markdown';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function fmtDateTime(iso: string): string {
  if (iso.length >= 16) return iso.slice(0, 16).replace('T', ' ');
  return iso;
}

function kindLabel(kind: string): string {
  if (kind === 'transcript_day') return 'Transcript Day';
  if (kind === 'transcript_course') return 'Course Transcript';
  return 'Note';
}

function statusColor(status: string): 'success' | 'warning' | 'error' | 'default' {
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

function sourceMetadataText(source: SourceRecord): string {
  const meta = source.metadata || {};
  const parts: string[] = [];
  if (source.kind === 'transcript_day' || source.kind === 'transcript_course') {
    if (meta.course_name) parts.push(`Course: ${String(meta.course_name)}`);
    if (source.kind === 'transcript_day' && meta.date) parts.push(`Date: ${String(meta.date)}`);
    if (source.kind === 'transcript_course' && meta.date_count != null) {
      parts.push(`${Number(meta.date_count)} dates`);
    }
  }
  if (meta.video_count != null) parts.push(`${Number(meta.video_count)} videos`);
  if (meta.segment_count != null) parts.push(`${Number(meta.segment_count)} segments`);
  if (source.kind === 'transcript_course' && meta.indexed_date_count != null) {
    parts.push(`indexed ${Number(meta.indexed_date_count)}/${meta.date_count != null ? Number(meta.date_count) : '?'}`);
  }
  if ((source.kind === 'transcript_day' || source.kind === 'transcript_course') && meta.processing_language) {
    parts.push(`language ${String(meta.processing_language)}`);
  }
  return parts.join(' | ');
}

function sourceNumberMeta(source: SourceRecord, key: string): number | null {
  const value = source.metadata?.[key];
  if (typeof value === 'number' && Number.isFinite(value)) return value;
  if (typeof value === 'string') {
    const parsed = Number(value);
    if (Number.isFinite(parsed)) return parsed;
  }
  return null;
}

function courseIndexProgress(source: SourceRecord) {
  if (source.kind !== 'transcript_course') return null;
  const indexed = sourceNumberMeta(source, 'indexed_date_count') ?? 0;
  const total = sourceNumberMeta(source, 'date_count');
  if (total == null || total <= 0) return null;
  const value = Math.max(0, Math.min(100, Math.round((indexed / total) * 100)));
  return { indexed, total, value };
}

// ---------------------------------------------------------------------------
// AddSourceModal
// ---------------------------------------------------------------------------

interface AddSourceModalProps {
  onClose: () => void;
  onCreated: () => void;
}

function AddSourceModal({ onClose, onCreated }: AddSourceModalProps) {
  const [mode, setMode] = useState<'transcript' | 'course' | 'note' | null>(null);
  const [courses, setCourses] = useState<CanvasCourse[]>([]);
  const [coursesLoading, setCoursesLoading] = useState(false);
  const [coursesError, setCoursesError] = useState<string | null>(null);

  // Transcript fields
  const [courseId, setCourseId] = useState('');
  const [courseName, setCourseName] = useState('');
  const [date, setDate] = useState('');
  const [processingLanguage, setProcessingLanguage] = useState('zh');
  const [dates, setDates] = useState<CourseDate[]>([]);
  const [datesLoading, setDatesLoading] = useState(false);
  const [datesError, setDatesError] = useState<string | null>(null);

  // Note fields
  const [noteName, setNoteName] = useState('');

  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [fileContent, setFileContent] = useState<string | null>(null);
  const [fileName, setFileName] = useState('');

  // Load courses when transcript mode is selected.
  useEffect(() => {
    if ((mode === 'transcript' || mode === 'course') && courses.length === 0 && !coursesLoading && !coursesError) {
      setCoursesLoading(true);
      api
        .getCanvasCourses()
        .then((res) => {
          if (res.status === 'succeeded') {
            setCourses(res.courses);
          } else {
            setCoursesError(res.error || 'Failed to load courses');
          }
        })
        .catch((err) => setCoursesError(String(err)))
        .finally(() => setCoursesLoading(false));
    }
  }, [mode, courses.length, coursesLoading, coursesError]);

  // Load dates when course is selected.
  useEffect(() => {
    if (courseId && (mode === 'transcript' || mode === 'course')) {
      setDatesLoading(true);
      setDatesError(null);
      setDates([]);
      setDate('');
      api
        .getCourseDates(courseId)
        .then((res) => {
          if (res.status === 'succeeded') {
            setDates(res.dates || []);
          } else {
            setDatesError(res.error || 'Failed to load dates');
          }
        })
        .catch((err) => setDatesError(String(err)))
        .finally(() => setDatesLoading(false));
    }
  }, [courseId, mode]);

  const handleCourseSelect = (val: string) => {
    setCourseId(val);
    if (val) {
      const course = courses.find((c) => String(c.id) === val);
      setCourseName(course?.name || '');
    } else {
      setCourseName('');
      setDates([]);
      setDate('');
    }
  };

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    setFileName(file.name);
    const base = file.name.replace(/\.[^.]+$/, '');
    if (!noteName) setNoteName(base);

    const reader = new FileReader();
    reader.onload = () => {
      setFileContent(reader.result as string);
    };
    reader.onerror = () => {
      setError('Failed to read file');
    };
    reader.readAsText(file);
  };

  const handleSubmitTranscript = async () => {
    if (!courseId) {
      setError('Please select a course.');
      return;
    }
    if (!date) {
      setError('Please select a date.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await api.createTranscriptDaySource({
        course_id: courseId,
        course_name: courseName || undefined,
        date,
        processing_language: processingLanguage,
      });
      if (res.status === 'failed') {
        setError(res.error || 'Creation failed');
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

  const handleSubmitCourse = async () => {
    if (!courseId) {
      setError('Please select a course.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await api.createTranscriptCourseSource({
        course_id: courseId,
        course_name: courseName || undefined,
        processing_language: processingLanguage,
      });
      if (res.status === 'failed') {
        setError(res.error || 'Creation failed');
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

  const handleSubmitNote = async () => {
    if (!fileContent) {
      setError('Please select a Markdown or text file.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await api.createNoteSource(noteName || fileName, fileContent);
      if (res.status === 'failed') {
        setError(res.error || 'Creation failed');
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
      <DialogTitle>Add Source</DialogTitle>
      <DialogContent>
        {!mode ? (
          <Stack spacing={2} sx={{ mt: 1 }}>
            <Typography variant="body2" color="text.secondary">
              Choose source type:
            </Typography>
            <Stack direction="row" spacing={2}>
              <Button variant="contained" onClick={() => setMode('transcript')}>
                Transcript Day
              </Button>
              <Button variant="outlined" onClick={() => setMode('course')}>
                Course Transcript
              </Button>
              <Button variant="outlined" onClick={() => setMode('note')}>
                Upload Notes
              </Button>
            </Stack>
          </Stack>
        ) : mode === 'transcript' || mode === 'course' ? (
          <Stack spacing={2.5} sx={{ mt: 1 }}>
            {coursesError && coursesError.includes('No Canvas') && (
              <Alert severity="info">
                <AlertTitle>Canvas Credentials Required</AlertTitle>
                Save Canvas credentials in <strong>Settings</strong> before creating
                transcript sources.
              </Alert>
            )}

            {coursesError && !coursesError.includes('No Canvas') && (
              <Alert severity="warning">{coursesError}</Alert>
            )}

            {coursesLoading ? (
              <LinearProgress />
            ) : (
              <FormControl fullWidth>
                <InputLabel>Course</InputLabel>
                <Select
                  value={courseId}
                  label="Course"
                  onChange={(e) => handleCourseSelect(e.target.value)}
                >
                  <MenuItem value="">
                    <em>-- Select a course --</em>
                  </MenuItem>
                  {courses.map((c) => (
                    <MenuItem key={c.id} value={String(c.id)}>
                      {c.course_code ? `${c.course_code} - ` : ''}
                      {c.name}
                      {c.term_name ? ` (${c.term_name})` : ''}
                    </MenuItem>
                  ))}
                </Select>
                {courses.length > 0 && (
                  <FormHelperText>
                    {courses.length} course(s) available
                  </FormHelperText>
                )}
              </FormControl>
            )}

            {courseId && (
              <>
                {datesLoading ? (
                  <Box sx={{ display: 'flex', alignItems: 'center', gap: 1 }}>
                    <CircularProgress size={16} />
                    <Typography variant="body2" color="text.secondary">
                      Loading available dates...
                    </Typography>
                  </Box>
                ) : datesError ? (
                  <Alert severity="error">
                    {datesError}
                    {datesError.includes('credential') && (
                      <span>
                        {' '}Go to <strong>Settings</strong> and save Canvas credentials.
                      </span>
                    )}
                  </Alert>
                ) : (
                  <FormControl fullWidth>
                    <InputLabel>{mode === 'course' ? 'Dates' : 'Date'}</InputLabel>
                    <Select
                      value={date}
                      label={mode === 'course' ? 'Dates' : 'Date'}
                      onChange={(e) => setDate(e.target.value)}
                      disabled={mode === 'course'}
                    >
                      <MenuItem value="">
                        <em>{mode === 'course' ? 'All dates in course' : '-- Select a date --'}</em>
                      </MenuItem>
                      {dates.map((d) => (
                        <MenuItem key={d.date} value={d.date}>
                          {d.date} — {d.video_count} video(s)
                          {d.first_title ? ` (${d.first_title}${d.last_title && d.last_title !== d.first_title ? ` ... ${d.last_title}` : ''})` : ''}
                        </MenuItem>
                      ))}
                    </Select>
                    <FormHelperText>
                      {mode === 'course'
                        ? `${dates.length} date(s), ${dates.reduce((sum, d) => sum + d.video_count, 0)} video(s) will be indexed`
                        : `${dates.length} date(s) with videos available`}
                    </FormHelperText>
                  </FormControl>
                )}
              </>
            )}

            <FormControl fullWidth>
              <InputLabel>Processing Language</InputLabel>
              <Select
                value={processingLanguage}
                label="Processing Language"
                onChange={(e) => setProcessingLanguage(e.target.value)}
              >
                <MenuItem value="zh">Chinese</MenuItem>
                <MenuItem value="en">English</MenuItem>
                <MenuItem value="bilingual">Bilingual</MenuItem>
              </Select>
              <FormHelperText>
                Controls generated summaries, keywords, and concepts.
              </FormHelperText>
            </FormControl>

            {error && (
              <Alert severity="error" onClose={() => setError(null)}>
                {error}
              </Alert>
            )}
          </Stack>
        ) : (
          <Stack spacing={2.5} sx={{ mt: 1 }}>
            <TextField
              label="Note Name (optional)"
              value={noteName}
              onChange={(e) => setNoteName(e.target.value)}
              placeholder="My Lecture Notes"
              fullWidth
            />

            <Box>
              <Button variant="outlined" component="label" startIcon={<UploadIcon />}>
                Select File (.md / .txt)
                <input
                  ref={fileInputRef}
                  type="file"
                  accept=".md,.txt,.markdown,.text"
                  hidden
                  onChange={handleFileChange}
                />
              </Button>
              {fileName && fileContent !== null && (
                <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5 }}>
                  {fileName} ({fileContent.length.toLocaleString()} chars)
                </Typography>
              )}
            </Box>

            {error && (
              <Alert severity="error" onClose={() => setError(null)}>
                {error}
              </Alert>
            )}
          </Stack>
        )}
      </DialogContent>
      <DialogActions>
        {mode === 'transcript' ? (
          <>
            <Button onClick={() => setMode(null)}>Back</Button>
            <Button
              variant="contained"
              onClick={handleSubmitTranscript}
              disabled={loading || !courseId || !date}
            >
              {loading ? 'Creating...' : 'Create Source'}
            </Button>
          </>
        ) : mode === 'course' ? (
          <>
            <Button onClick={() => setMode(null)}>Back</Button>
            <Button
              variant="contained"
              onClick={handleSubmitCourse}
              disabled={loading || !courseId}
            >
              {loading ? 'Creating...' : 'Create Course Source'}
            </Button>
          </>
        ) : mode === 'note' ? (
          <>
            <Button onClick={() => setMode(null)}>Back</Button>
            <Button
              variant="contained"
              onClick={handleSubmitNote}
              disabled={loading || fileContent === null}
            >
              {loading ? 'Uploading...' : 'Create Source'}
            </Button>
          </>
        ) : (
          <Button onClick={onClose}>Cancel</Button>
        )}
      </DialogActions>
    </Dialog>
  );
}

// ---------------------------------------------------------------------------
// AskInline (streaming)
// ---------------------------------------------------------------------------

interface AskInlineProps {
  sourceId: string;
}

function AskInline({ sourceId }: AskInlineProps) {
  const [question, setQuestion] = useState('');
  const [answer, setAnswer] = useState('');
  const [llmUsed, setLlmUsed] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);
  const eventSourceRef = useRef<EventSource | null>(null);

  const handleAsk = () => {
    if (!question.trim()) return;
    // Clean up previous EventSource.
    if (eventSourceRef.current) {
      eventSourceRef.current.close();
      eventSourceRef.current = null;
    }
    setLoading(true);
    setError(null);
    setAnswer('');
    setDone(false);
    setLlmUsed(false);

    const url = api.askStreamUrl(sourceId, question);
    const es = new EventSource(url);
    eventSourceRef.current = es;

    es.addEventListener('meta', (e: MessageEvent) => {
      try {
        const meta = JSON.parse(e.data);
        setLlmUsed(meta.llm_used);
      } catch {
        // ignore parse errors
      }
    });

    es.addEventListener('chunk', (e: MessageEvent) => {
      setAnswer((prev) => prev + e.data);
    });

    es.addEventListener('done', () => {
      setLoading(false);
      setDone(true);
      es.close();
      eventSourceRef.current = null;
    });

    es.addEventListener('error', (e: MessageEvent) => {
      if (e.data) {
        setError(e.data);
      } else if (es.readyState === EventSource.CLOSED) {
        // Connection closed normally after done.
        setLoading(false);
        setDone(true);
      } else {
        setError('Connection error. The server may have closed the stream.');
        setLoading(false);
      }
      es.close();
      eventSourceRef.current = null;
    });

    es.onerror = () => {
      if (es.readyState === EventSource.CLOSED) {
        setLoading(false);
        setDone(true);
      }
    };
  };

  // Cleanup on unmount.
  useEffect(() => {
    return () => {
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
      }
    };
  }, []);

  return (
    <Box sx={{ mt: 2 }}>
      <Stack direction="row" spacing={1} alignItems="center">
        <TextField
          size="small"
          fullWidth
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !loading) handleAsk();
          }}
          placeholder="Ask a question about this source..."
        />
        <Button
          variant="contained"
          size="small"
          onClick={handleAsk}
          disabled={loading || !question.trim()}
          sx={{ whiteSpace: 'nowrap', minWidth: 64 }}
        >
          {loading ? <CircularProgress size={16} /> : 'Ask'}
        </Button>
      </Stack>

      {error && (
        <Alert severity="error" sx={{ mt: 1 }} onClose={() => setError(null)}>
          {error}
        </Alert>
      )}

      {answer && (
        <Card variant="outlined" sx={{ mt: 1, maxHeight: 400, overflow: 'auto', p: 2 }}>
          {!llmUsed && !loading && (
            <Typography variant="caption" color="text.secondary" gutterBottom sx={{ display: 'block', mb: 1 }}>
              Deterministic search (LLM not available)
            </Typography>
          )}
          <Box className="markdown-body" sx={{ fontSize: '0.875rem' }}>
            <ReactMarkdown>{answer}</ReactMarkdown>
          </Box>
          {loading && !done && <LinearProgress sx={{ mt: 1 }} />}
        </Card>
      )}
    </Box>
  );
}

// ---------------------------------------------------------------------------
// UpdateNoteModal
// ---------------------------------------------------------------------------

interface UpdateNoteModalProps {
  source: SourceRecord;
  onClose: () => void;
  onUpdated: () => void;
}

function UpdateNoteModal({ source, onClose, onUpdated }: UpdateNoteModalProps) {
  const [name, setName] = useState(source.title);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [fileContent, setFileContent] = useState<string | null>(null);
  const [fileName, setFileName] = useState('');

  const handleFileChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (!file) return;
    setFileName(file.name);
    const reader = new FileReader();
    reader.onload = () => {
      setFileContent(reader.result as string);
    };
    reader.onerror = () => {
      setError('Failed to read file');
    };
    reader.readAsText(file);
  };

  const handleSubmit = async () => {
    if (!fileContent) {
      setError('Please select a file.');
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const res = await api.updateNoteSource(source.id, name, fileContent);
      if (res.status === 'failed') {
        setError(res.error || 'Update failed');
      } else {
        onUpdated();
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
      <DialogTitle>Update Note</DialogTitle>
      <DialogContent>
        <Stack spacing={2.5} sx={{ mt: 1 }}>
          <TextField
            label="Note Name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            fullWidth
          />
          <Box>
            <Button variant="outlined" component="label" startIcon={<UploadIcon />}>
              Upload New Version (.md / .txt)
              <input
                ref={fileInputRef}
                type="file"
                accept=".md,.txt,.markdown,.text"
                hidden
                onChange={handleFileChange}
              />
            </Button>
            {fileName && fileContent !== null && (
              <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5 }}>
                {fileName} ({fileContent.length.toLocaleString()} chars)
              </Typography>
            )}
          </Box>
          {error && (
            <Alert severity="error" onClose={() => setError(null)}>
              {error}
            </Alert>
          )}
        </Stack>
      </DialogContent>
      <DialogActions>
        <Button onClick={onClose}>Cancel</Button>
        <Button
          variant="contained"
          onClick={handleSubmit}
          disabled={loading || fileContent === null}
        >
          {loading ? 'Updating...' : 'Update'}
        </Button>
      </DialogActions>
    </Dialog>
  );
}

interface IndexOverviewProps {
  sourceId: string;
}

function IndexOverview({ sourceId }: IndexOverviewProps) {
  const [indexes, setIndexes] = useState<CourseDateIndex[]>([]);
  const [expandedDate, setExpandedDate] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const load = (showLoading: boolean) => {
      if (showLoading) setLoading(true);
      api
        .getSourceIndex(sourceId)
        .then((res) => {
          if (!cancelled) {
            setIndexes(res.indexes || []);
            setError(res.error || null);
          }
        })
        .catch((e) => {
          if (!cancelled) setError(String(e));
        })
        .finally(() => {
          if (!cancelled) setLoading(false);
        });
    };
    load(true);
    const interval = setInterval(() => load(false), 3000);
    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [sourceId]);

  const needle = query.trim().toLowerCase();
  const filtered = indexes.filter((idx) => {
    if (!needle) return true;
    return [
      idx.date,
      idx.title,
      idx.summary,
      idx.keywords.join(' '),
      idx.concepts.join(' '),
    ].join(' ').toLowerCase().includes(needle);
  });

  if (loading) return <LinearProgress sx={{ mt: 1 }} />;
  if (error) return <Alert severity="warning" sx={{ mt: 1 }}>{error}</Alert>;

  return (
    <Box sx={{ mt: 1.5 }}>
      <TextField
        size="small"
        label="Search index"
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        fullWidth
        sx={{ mb: 1 }}
      />
      <Table size="small">
        <TableHead>
          <TableRow>
            <TableCell>Date</TableCell>
            <TableCell>Summary</TableCell>
            <TableCell>Keywords</TableCell>
            <TableCell align="right">Chars</TableCell>
            <TableCell align="right">Tokens</TableCell>
            <TableCell>Status</TableCell>
          </TableRow>
        </TableHead>
        <TableBody>
          {filtered.map((idx) => (
            <Fragment key={idx.date}>
              <TableRow
                hover
                sx={{ cursor: 'pointer' }}
                onClick={() => setExpandedDate(expandedDate === idx.date ? null : idx.date)}
              >
                <TableCell>{idx.date}</TableCell>
                <TableCell>{idx.summary.slice(0, 140)}</TableCell>
                <TableCell>{idx.keywords.slice(0, 5).join(', ')}</TableCell>
                <TableCell align="right">{idx.char_count.toLocaleString()}</TableCell>
                <TableCell align="right">{idx.token_count.toLocaleString()}</TableCell>
                <TableCell>{idx.status}</TableCell>
              </TableRow>
              {expandedDate === idx.date && (
                <TableRow>
                  <TableCell colSpan={6}>
                    <Typography variant="body2" sx={{ mb: 0.5 }}>
                      {idx.summary}
                    </Typography>
                    <Typography variant="caption" color="text.secondary">
                      Concepts: {idx.concepts.join(', ') || 'None'}
                    </Typography>
                    <Box sx={{ mt: 1 }}>
                      {idx.timestamp_ranges.slice(0, 6).map((r, i) => (
                        <Typography key={i} variant="caption" display="block" color="text.secondary">
                          {r.video_id} [{Math.round(r.start)}-{Math.round(r.end)}s] {r.text_preview}
                        </Typography>
                      ))}
                    </Box>
                  </TableCell>
                </TableRow>
              )}
            </Fragment>
          ))}
        </TableBody>
      </Table>
    </Box>
  );
}

// ---------------------------------------------------------------------------
// Sources page
// ---------------------------------------------------------------------------

export default function Sources() {
  const [sources, setSources] = useState<SourceRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showAddModal, setShowAddModal] = useState(false);
  const [updateTarget, setUpdateTarget] = useState<SourceRecord | null>(null);

  // Ask state: source id -> open
  const [askOpen, setAskOpen] = useState<Set<string>>(new Set());
  const [indexOpen, setIndexOpen] = useState<Set<string>>(new Set());

  // Job-polling state: source id -> job info
  const [activeJobs, setActiveJobs] = useState<Map<string, string>>(new Map());

  const loadSources = useCallback(async () => {
    try {
      const res = await api.getSources();
      setSources(res.sources);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadSources();
  }, [loadSources]);

  // Poll active jobs.
  useEffect(() => {
    if (activeJobs.size === 0) return;
    const cancelled = { value: false };

    const poll = async () => {
      for (const [sourceId, jobId] of activeJobs) {
        try {
          const job = await api.getJob(jobId);
          if (job.status === 'succeeded' || job.status === 'failed') {
            setActiveJobs((prev) => {
              const next = new Map(prev);
              next.delete(sourceId);
              return next;
            });
            loadSources();
          }
        } catch {
          try {
            const job = await api.getJobLegacy(jobId);
            if (job.status === 'succeeded' || job.status === 'failed') {
              setActiveJobs((prev) => {
                const next = new Map(prev);
                next.delete(sourceId);
                return next;
              });
              loadSources();
            }
          } catch {
            // Keep polling.
          }
        }
      }
      if (!cancelled.value && activeJobs.size > 0) {
        setTimeout(poll, 2000);
      }
    };
    poll();

    return () => {
      cancelled.value = true;
    };
  }, [activeJobs, loadSources]);

  // Periodically refresh while any source is processing.
  const hasProcessing = sources.some((s) => s.status === 'processing');
  useEffect(() => {
    if (!hasProcessing) return;
    const interval = setInterval(loadSources, 3000);
    return () => clearInterval(interval);
  }, [hasProcessing, loadSources]);

  const handleCreated = () => {
    loadSources();
  };

  const handleDelete = async (source: SourceRecord) => {
    if (!window.confirm(`Delete source "${source.title}"? This does not delete files.`)) return;
    try {
      await api.deleteSource(source.id);
      loadSources();
    } catch (e) {
      setError(String(e));
    }
  };

  const handleSync = async (source: SourceRecord) => {
    try {
      const res = await api.syncSource(source.id);
      if (res.status === 'processing' && res.job_id && res.source_id) {
        setActiveJobs((prev) => {
          const next = new Map(prev);
          next.set(res.source_id!, res.job_id!);
          return next;
        });
      } else if (res.status === 'failed') {
        setError(res.error || 'Sync failed');
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const handleReindex = async (source: SourceRecord) => {
    try {
      const res = await api.reindexSource(source.id);
      if (res.status === 'processing' && res.job_id && res.source_id) {
        setActiveJobs((prev) => {
          const next = new Map(prev);
          next.set(res.source_id!, res.job_id!);
          return next;
        });
      } else if (res.status === 'failed') {
        setError(res.error || 'Reindex failed');
      } else {
        loadSources();
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const toggleAsk = (sourceId: string) => {
    setAskOpen((prev) => {
      const next = new Set(prev);
      if (next.has(sourceId)) {
        next.delete(sourceId);
      } else {
        next.add(sourceId);
      }
      return next;
    });
  };

  const toggleIndex = (sourceId: string) => {
    setIndexOpen((prev) => {
      const next = new Set(prev);
      if (next.has(sourceId)) next.delete(sourceId);
      else next.add(sourceId);
      return next;
    });
  };

  return (
    <Box>
      {/* Header */}
      <Stack direction="row" justifyContent="space-between" alignItems="center" sx={{ mb: 3 }}>
        <Typography variant="h4" component="h1">Sources</Typography>
        <Button variant="contained" startIcon={<AddIcon />} onClick={() => setShowAddModal(true)}>
          Add Source
        </Button>
      </Stack>

      {error && (
        <Alert severity="error" sx={{ mb: 2 }} onClose={() => setError(null)}>
          {error}
        </Alert>
      )}

      {/* Add modal */}
      {showAddModal && (
        <AddSourceModal
          onClose={() => setShowAddModal(false)}
          onCreated={handleCreated}
        />
      )}

      {/* Update note modal */}
      {updateTarget && (
        <UpdateNoteModal
          source={updateTarget}
          onClose={() => setUpdateTarget(null)}
          onUpdated={handleCreated}
        />
      )}

      {/* Source list */}
      {loading ? (
        <Stack alignItems="center" sx={{ py: 4 }}>
          <CircularProgress />
          <Typography color="text.secondary" sx={{ mt: 1 }}>Loading sources...</Typography>
        </Stack>
      ) : sources.length === 0 ? (
        <Card>
          <CardContent>
            <Typography color="text.secondary">
              No sources yet. Click "Add Source" to import Canvas transcripts or upload notes.
            </Typography>
          </CardContent>
        </Card>
      ) : (
        <Stack spacing={2}>
          {sources.map((source) => (
            <Card key={source.id}>
              <CardContent sx={{ pb: 1 }}>
                <Stack direction="row" justifyContent="space-between" alignItems="flex-start" spacing={2}>
                  <Box sx={{ flex: 1 }}>
                    <Stack direction="row" spacing={0.5} alignItems="center" sx={{ mb: 0.5 }}>
                      <Chip
                        label={source.status}
                        size="small"
                        color={statusColor(source.status)}
                        variant="filled"
                      />
                      <Chip
                        label={kindLabel(source.kind)}
                        size="small"
                        variant="outlined"
                      />
                      <Typography variant="h6" component="h2" sx={{ fontSize: '1rem' }}>
                        {source.title}
                      </Typography>
                    </Stack>
                    <Typography variant="body2" color="text.secondary">
                      Created: {fmtDateTime(source.created_at)}
                      {source.updated_at !== source.created_at &&
                        ` | Updated: ${fmtDateTime(source.updated_at)}`}
                      {source.length && ` | ${source.length}`}
                    </Typography>
                    {source.last_error && (
                      <Typography variant="body2" color="error" sx={{ mt: 0.5 }}>
                        Error: {source.last_error}
                      </Typography>
                    )}
                    {source.metadata &&
                      typeof source.metadata === 'object' &&
                      Object.keys(source.metadata as Record<string, unknown>).length > 0 && (
                        <Typography variant="body2" color="text.secondary" sx={{ mt: 0.5 }}>
                          {sourceMetadataText(source)}
                        </Typography>
                      )}
                    {source.status === 'processing' && (() => {
                      const progress = courseIndexProgress(source);
                      if (!progress) return null;
                      return (
                        <Box sx={{ mt: 1 }}>
                          <LinearProgress variant="determinate" value={progress.value} />
                          <Typography variant="caption" color="text.secondary">
                            Indexing {progress.indexed}/{progress.total} dates · {progress.value}%
                          </Typography>
                        </Box>
                      );
                    })()}
                    {source.job_id && source.status === 'processing' && (
                      <Box sx={{ mt: 1 }}>
                        {source.kind !== 'transcript_course' && <LinearProgress />}
                        <Typography variant="caption" color="text.secondary">
                          Job: {source.job_id.slice(0, 8)}... (processing)
                        </Typography>
                      </Box>
                    )}
                  </Box>

                  {/* Actions */}
                  <Stack direction="row" spacing={0.5} sx={{ flexShrink: 0 }}>
                    {(source.kind === 'transcript_day' || source.kind === 'transcript_course') && (
                      <Tooltip title="Sync">
                        <IconButton
                          size="small"
                          onClick={() => handleSync(source)}
                          disabled={source.status === 'processing'}
                        >
                          <SyncIcon />
                        </IconButton>
                      </Tooltip>
                    )}
                    {source.kind === 'transcript_course' && (
                      <>
                        <Tooltip title="Index Overview">
                          <IconButton size="small" onClick={() => toggleIndex(source.id)}>
                            <ExpandMoreIcon />
                          </IconButton>
                        </Tooltip>
                        <Tooltip title="Reindex">
                          <IconButton
                            size="small"
                            onClick={() => handleReindex(source)}
                            disabled={source.status === 'processing'}
                          >
                            <RefreshIcon />
                          </IconButton>
                        </Tooltip>
                      </>
                    )}
                    {source.kind === 'note' && (
                      <Tooltip title="Update">
                        <IconButton size="small" onClick={() => setUpdateTarget(source)}>
                          <UploadIcon />
                        </IconButton>
                      </Tooltip>
                    )}
                    <Tooltip title="Ask">
                      <IconButton size="small" onClick={() => toggleAsk(source.id)}>
                        <QuestionAnswerIcon />
                      </IconButton>
                    </Tooltip>
                    <Tooltip title="Delete">
                      <IconButton size="small" color="error" onClick={() => handleDelete(source)}>
                        <DeleteIcon />
                      </IconButton>
                    </Tooltip>
                  </Stack>
                </Stack>
              </CardContent>

              {/* Ask inline */}
              {askOpen.has(source.id) && (
                <CardContent sx={{ pt: 0 }}>
                  <AskInline sourceId={source.id} />
                </CardContent>
              )}
              {source.kind === 'transcript_course' && indexOpen.has(source.id) && (
                <CardContent sx={{ pt: 0 }}>
                  <IndexOverview sourceId={source.id} />
                </CardContent>
              )}
            </Card>
          ))}
        </Stack>
      )}
    </Box>
  );
}
