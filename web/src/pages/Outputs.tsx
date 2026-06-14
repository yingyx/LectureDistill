import { useEffect, useState } from 'react';
import { api, type OutputFile } from '../api';
import {
  Button,
  Card,
  CardContent,
  Typography,
  Table,
  TableHead,
  TableBody,
  TableRow,
  TableCell,
  Chip,
  Stack,
  TextField,
  Alert,
  Box,
  CircularProgress,
} from '@mui/material';
import RefreshIcon from '@mui/icons-material/Refresh';

export default function Outputs() {
  const [outputs, setOutputs] = useState<Record<string, OutputFile[]>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // Diff state
  const [diffBase, setDiffBase] = useState('notes.md');
  const [diffPatched, setDiffPatched] = useState('artifacts/notes/notes.patched.md');
  const [diffResult, setDiffResult] = useState<string | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);

  const loadOutputs = async () => {
    setLoading(true);
    try {
      const res = await api.getOutputs();
      setOutputs(res.outputs);
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    loadOutputs();
  }, []);

  const handleDiff = async () => {
    setDiffLoading(true);
    setDiffError(null);
    setDiffResult(null);
    try {
      const res = await api.getNotesDiff(diffBase, diffPatched);
      setDiffResult(res.unified || JSON.stringify(res.hunks, null, 2));
    } catch (e) {
      setDiffError(String(e));
    } finally {
      setDiffLoading(false);
    }
  };

  const categoryOrder = ['transcripts', 'notes', 'outputs', 'other'];
  const sortedCategories = Object.keys(outputs).sort(
    (a, b) => categoryOrder.indexOf(a) - categoryOrder.indexOf(b),
  );

  if (error) {
    return <Alert severity="error">Failed to load outputs: {error}</Alert>;
  }

  return (
    <Box>
      <Stack direction="row" justifyContent="space-between" alignItems="center" sx={{ mb: 2 }}>
        <Typography variant="h4" component="h1">Outputs</Typography>
        <Button
          variant="outlined"
          size="small"
          startIcon={<RefreshIcon />}
          onClick={loadOutputs}
          disabled={loading}
        >
          {loading ? 'Loading...' : 'Refresh'}
        </Button>
      </Stack>

      {sortedCategories.length === 0 && !loading ? (
        <Card>
          <CardContent>
            <Typography color="text.secondary">
              No output files found. Run pipeline stages to generate artifacts.
            </Typography>
          </CardContent>
        </Card>
      ) : loading ? (
        <Stack alignItems="center" sx={{ py: 4 }}>
          <CircularProgress />
        </Stack>
      ) : (
        <Stack spacing={2}>
          {sortedCategories.map((category) => {
            const files = outputs[category];
            return (
              <Card key={category}>
                <CardContent sx={{ pb: 1 }}>
                  <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 1.5 }}>
                    <Typography variant="h6" sx={{ fontSize: '1rem', textTransform: 'capitalize' }}>
                      {category}
                    </Typography>
                    <Chip label={files.length} size="small" variant="outlined" />
                  </Stack>
                  <Table size="small">
                    <TableHead>
                      <TableRow>
                        <TableCell>File</TableCell>
                        <TableCell>Path</TableCell>
                        <TableCell align="right">Size</TableCell>
                      </TableRow>
                    </TableHead>
                    <TableBody>
                      {files.map((f, i) => (
                        <TableRow key={i}>
                          <TableCell>
                            <Typography variant="body2" component="code" sx={{ fontFamily: 'monospace' }}>
                              {f.name}
                            </Typography>
                          </TableCell>
                          <TableCell>
                            <Typography variant="body2" color="text.secondary" component="code" sx={{ fontFamily: 'monospace', fontSize: '0.75rem' }}>
                              {f.path}
                            </Typography>
                          </TableCell>
                          <TableCell align="right">
                            <Typography variant="body2" color="text.secondary">
                              {f.size.toLocaleString()} B
                            </Typography>
                          </TableCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </Table>
                </CardContent>
              </Card>
            );
          })}
        </Stack>
      )}

      {/* Notes Diff */}
      <Card sx={{ mt: 3 }}>
        <CardContent>
          <Typography variant="h6" sx={{ fontSize: '1rem', mb: 1 }}>
            Notes Diff
          </Typography>
          <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
            Compare original notes with patched notes using the deterministic line diff.
          </Typography>
          <Stack direction="row" spacing={2} alignItems="flex-start" sx={{ mb: 2 }}>
            <TextField
              label="Base (original)"
              size="small"
              value={diffBase}
              onChange={(e) => setDiffBase(e.target.value)}
              sx={{ flex: 1 }}
            />
            <TextField
              label="Patched"
              size="small"
              value={diffPatched}
              onChange={(e) => setDiffPatched(e.target.value)}
              sx={{ flex: 1 }}
            />
            <Button
              variant="contained"
              onClick={handleDiff}
              disabled={diffLoading}
              sx={{ mt: 0 }}
            >
              {diffLoading ? 'Diffing...' : 'Diff'}
            </Button>
          </Stack>

          {diffError && <Alert severity="error" sx={{ mb: 1 }}>{diffError}</Alert>}

          {diffResult && (
            <Box
              component="pre"
              sx={{
                bgcolor: 'grey.100',
                p: 2,
                borderRadius: 1,
                maxHeight: 400,
                overflow: 'auto',
                fontSize: '0.75rem',
                fontFamily: 'monospace',
                whiteSpace: 'pre-wrap',
                m: 0,
              }}
            >
              {diffResult}
            </Box>
          )}
        </CardContent>
      </Card>
    </Box>
  );
}
