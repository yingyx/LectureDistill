import { useEffect, useState } from 'react';
import { api, type SecretStatus, type PluginDescriptor } from '../api';
import {
  Button,
  Card,
  CardContent,
  CardActions,
  Typography,
  TextField,
  Chip,
  Stack,
  Alert,
  Box,
  Divider,
} from '@mui/material';
import KeyIcon from '@mui/icons-material/Key';
import CloudIcon from '@mui/icons-material/Cloud';
import AccountCircleIcon from '@mui/icons-material/AccountCircle';
import PictureAsPdfIcon from '@mui/icons-material/PictureAsPdf';

const emptyStatus: SecretStatus = {
  llm: {
    api_key_set: false,
    api_key_masked: '',
    base_url: '',
    model: '',
  },
  canvas: {
    token_set: false,
    token_masked: '',
    cookie_set: false,
    cookie_masked: '',
  },
  jaccount: {
    cookie_set: false,
    cookie_masked: '',
  },
};

function SecretBadge({ set, masked }: { set: boolean; masked: string }) {
  if (!set) return <Chip label="Not saved" size="small" color="default" variant="outlined" />;
  return <Chip label={`Saved ${masked}`} size="small" color="success" />;
}

type RefCheatTemplate = {
  name: string;
  path: string;
  calibrated_at?: string;
};

export default function Settings() {
  const [status, setStatus] = useState<SecretStatus>(emptyStatus);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const [llmApiKey, setLlmApiKey] = useState('');
  const [llmBaseUrl, setLlmBaseUrl] = useState('');
  const [llmModel, setLlmModel] = useState('');
  const [canvasToken, setCanvasToken] = useState('');
  const [canvasCookie, setCanvasCookie] = useState('');
  const [jaccountCookie, setJaccountCookie] = useState('');
  const [llmMaxConcurrency, setLlmMaxConcurrency] = useState('2');
  const [typstPath, setTypstPath] = useState('');
  const [plugins, setPlugins] = useState<PluginDescriptor[]>([]);
  const [pluginConfig, setPluginConfig] = useState<Record<string, Record<string, unknown>>>({});
  const [refCheatTemplatePath, setRefCheatTemplatePath] = useState('');
  const [pluginActionBusy, setPluginActionBusy] = useState(false);

  const loadSecrets = async () => {
    try {
      const res = await api.getSecrets();
      setStatus(res.secrets);
      setLlmBaseUrl(res.secrets.llm.base_url || '');
      setLlmModel(res.secrets.llm.model || '');
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  };

  const loadConfig = async () => {
    try {
      const res = await api.getState();
      const value = res.config?.llm_max_concurrency;
      setLlmMaxConcurrency(String(typeof value === 'number' ? value : 2));
      setTypstPath(typeof res.config?.typst_path === 'string' ? res.config.typst_path : '');
    } catch {
      setLlmMaxConcurrency('2');
      setTypstPath('');
    }
  };

  const loadPlugins = async () => {
    try {
      const res = await api.getPlugins();
      setPlugins(res.plugins || []);
      setPluginConfig(res.config || {});
    } catch {
      setPlugins([]);
      setPluginConfig({});
    }
  };

  useEffect(() => {
    loadSecrets();
    loadConfig();
    loadPlugins();
  }, []);

  const save = async (clear: string[] = []) => {
    setSaving(true);
    setError(null);
    setMessage(null);
    try {
      const fields: Record<string, string> = {
        llm_api_key: llmApiKey,
        llm_base_url: llmBaseUrl,
        llm_model: llmModel,
        canvas_token: canvasToken,
        canvas_cookie: canvasCookie,
        jaccount_cookie: jaccountCookie,
      };
      const res = await api.patchSecrets(fields, clear);
      if (res.status !== 'ok') throw new Error(res.error || 'Failed to save credentials');
      setStatus(res.secrets);
      setLlmApiKey('');
      setCanvasToken('');
      setCanvasCookie('');
      setJaccountCookie('');
      setMessage('Credentials saved locally.');
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const clearField = async (field: string) => {
    await save([field]);
  };

  const saveRuntimeConfig = async () => {
    setSaving(true);
    setError(null);
    setMessage(null);
    try {
      const parsed = Math.max(1, Math.min(32, Number(llmMaxConcurrency) || 2));
      const res = await api.patchState({
        llm_max_concurrency: parsed,
        typst_path: typstPath.trim(),
      });
      if (typeof res === 'object' && res && 'status' in res && (res as { status?: string }).status === 'error') {
        throw new Error((res as { error?: string }).error || 'Failed to save runtime config');
      }
      setLlmMaxConcurrency(String(parsed));
      setTypstPath(typstPath.trim());
      setMessage('Runtime configuration saved.');
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  const refCheatTemplates = (): RefCheatTemplate[] => {
    const templates = pluginConfig['builtin.ref_cheat']?.templates;
    if (!Array.isArray(templates)) return [];
    const parsed: RefCheatTemplate[] = [];
    for (const template of templates) {
      if (!template || typeof template !== 'object') continue;
      const item = template as Record<string, unknown>;
      const name = typeof item.name === 'string' ? item.name : '';
      const path = typeof item.path === 'string' ? item.path : '';
      if (!name || !path) continue;
      const calibrated = typeof item.calibrated_at === 'string' ? item.calibrated_at : undefined;
      parsed.push({ name, path, calibrated_at: calibrated });
    }
    return parsed;
  };

  const refCheatDefaultTemplate = () => {
    const value = pluginConfig['builtin.ref_cheat']?.default_template;
    return typeof value === 'string' ? value : '';
  };

  const runRefCheatAction = async (action: string, fields: Record<string, unknown>) => {
    setPluginActionBusy(true);
    setError(null);
    setMessage(null);
    try {
      const res = await api.pluginAction('builtin.ref_cheat', action, fields);
      if (res.status !== 'ok') throw new Error(res.error || `Plugin action failed: ${action}`);
      await loadPlugins();
      setMessage(`Plugin action completed: ${action}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setPluginActionBusy(false);
    }
  };

  return (
    <Box>
      <Typography variant="h4" component="h1" sx={{ mb: 3 }}>Settings</Typography>

      {message && (
        <Alert severity="success" sx={{ mb: 2 }} onClose={() => setMessage(null)}>
          {message}
        </Alert>
      )}
      {error && (
        <Alert severity="error" sx={{ mb: 2 }} onClose={() => setError(null)}>
          {error}
        </Alert>
      )}

      <Stack spacing={3}>
        {/* LLM Provider */}
        <Card>
          <CardContent>
            <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 2 }}>
              <KeyIcon color="primary" />
              <Typography variant="h6" sx={{ fontSize: '1.1rem' }}>LLM Provider</Typography>
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
              Stored values are used as OPENAI_API_KEY, OPENAI_BASE_URL, and OPENAI_MODEL
              for note completion, distillation jobs, and source Q&A.
            </Typography>
            <Stack spacing={2}>
              <Box>
                <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 0.5 }}>
                  <Typography variant="body2" fontWeight={500}>API Key</Typography>
                  <SecretBadge set={status.llm.api_key_set} masked={status.llm.api_key_masked} />
                </Stack>
                <TextField
                  type="password"
                  size="small"
                  fullWidth
                  value={llmApiKey}
                  onChange={(e) => setLlmApiKey(e.target.value)}
                  placeholder="Leave blank to keep existing key"
                />
              </Box>
              <TextField
                label="Base URL"
                size="small"
                fullWidth
                value={llmBaseUrl}
                onChange={(e) => setLlmBaseUrl(e.target.value)}
                placeholder="https://api.openai.com/v1"
              />
              <TextField
                label="Model"
                size="small"
                fullWidth
                value={llmModel}
                onChange={(e) => setLlmModel(e.target.value)}
                placeholder="gpt-4o-mini"
              />
              <TextField
                label="Max Concurrent LLM Requests"
                type="number"
                size="small"
                fullWidth
                value={llmMaxConcurrency}
                onChange={(e) => setLlmMaxConcurrency(e.target.value)}
                inputProps={{ min: 1, max: 32 }}
                helperText="Shared across project jobs from request start until model response completion."
              />
            </Stack>
          </CardContent>
          <CardActions sx={{ px: 2, pb: 2 }}>
            <Button variant="contained" size="small" onClick={() => save()} disabled={saving}>
              Save LLM
            </Button>
            <Button variant="outlined" size="small" onClick={saveRuntimeConfig} disabled={saving}>
              Save Runtime
            </Button>
            <Button variant="outlined" size="small" onClick={() => clearField('llm_api_key')} disabled={saving} color="error">
              Clear Key
            </Button>
          </CardActions>
        </Card>

        {/* PDF Renderer */}
        <Card>
          <CardContent>
            <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 2 }}>
              <PictureAsPdfIcon color="primary" />
              <Typography variant="h6" sx={{ fontSize: '1.1rem' }}>PDF Renderer</Typography>
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
              Typst is preferred for cheat sheet rendering. Leave the path empty to use typst from PATH.
            </Typography>
            <TextField
              label="Typst Executable Path"
              size="small"
              fullWidth
              value={typstPath}
              onChange={(e) => setTypstPath(e.target.value)}
              placeholder="typst or C:\\path\\to\\typst.exe"
            />
          </CardContent>
          <CardActions sx={{ px: 2, pb: 2 }}>
            <Button variant="outlined" size="small" onClick={saveRuntimeConfig} disabled={saving}>
              Save Renderer
            </Button>
          </CardActions>
        </Card>

        {/* Canvas */}
        <Card>
          <CardContent>
            <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 2 }}>
              <CloudIcon color="primary" />
              <Typography variant="h6" sx={{ fontSize: '1.1rem' }}>Canvas</Typography>
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
              Canvas credentials are required for listing courses, fetching video metadata,
              and ingesting subtitles. Save a Canvas JAAuthCookie here for video access.
            </Typography>
            <Stack spacing={2}>
              <Box>
                <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 0.5 }}>
                  <Typography variant="body2" fontWeight={500}>Canvas Token</Typography>
                  <SecretBadge set={status.canvas.token_set} masked={status.canvas.token_masked} />
                </Stack>
                <TextField
                  type="password"
                  size="small"
                  fullWidth
                  value={canvasToken}
                  onChange={(e) => setCanvasToken(e.target.value)}
                  placeholder="Optional Canvas API token"
                />
              </Box>
              <Box>
                <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 0.5 }}>
                  <Typography variant="body2" fontWeight={500}>JAAuthCookie (video access)</Typography>
                  <SecretBadge set={status.canvas.cookie_set} masked={status.canvas.cookie_masked} />
                </Stack>
                <TextField
                  type="password"
                  size="small"
                  fullWidth
                  value={canvasCookie}
                  onChange={(e) => setCanvasCookie(e.target.value)}
                  placeholder="Leave blank to keep existing cookie"
                />
              </Box>
            </Stack>
          </CardContent>
          <CardActions sx={{ px: 2, pb: 2 }}>
            <Button variant="contained" size="small" onClick={() => save()} disabled={saving}>
              Save Canvas
            </Button>
            <Button variant="outlined" size="small" onClick={() => clearField('canvas_cookie')} disabled={saving} color="error">
              Clear Cookie
            </Button>
          </CardActions>
        </Card>

        {/* Plugins */}
        <Card>
          <CardContent>
            <Typography variant="h6" sx={{ fontSize: '1.1rem', mb: 2 }}>Plugins</Typography>
            <Stack spacing={1.5}>
              {plugins.map((plugin) => (
                <Box key={plugin.id} sx={{ border: 1, borderColor: 'divider', borderRadius: 1, p: 1.5 }}>
                  <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 0.5 }}>
                    <Typography variant="body2" fontWeight={600}>{plugin.display_name}</Typography>
                    <Chip label={plugin.kind} size="small" variant="outlined" />
                    <Chip label={plugin.id} size="small" />
                  </Stack>
                  {plugin.nodes.length > 0 && (
                    <Typography variant="caption" color="text.secondary" display="block">
                      Nodes: {plugin.nodes.map((node) => node.key).join(', ')}
                    </Typography>
                  )}
                  {plugin.actions.length > 0 && (
                    <Typography variant="caption" color="text.secondary" display="block">
                      Actions: {plugin.actions.join(', ')}
                    </Typography>
                  )}
                  {pluginConfig[plugin.id] && Object.keys(pluginConfig[plugin.id]).length > 0 && (
                    <Box component="pre" sx={{ mt: 1, mb: 0, fontSize: '0.75rem', whiteSpace: 'pre-wrap' }}>
                      {JSON.stringify(pluginConfig[plugin.id], null, 2)}
                    </Box>
                  )}
                  {plugin.id === 'builtin.ref_cheat' && (
                    <Box sx={{ mt: 1.5 }}>
                      <Stack direction={{ xs: 'column', sm: 'row' }} spacing={1}>
                        <TextField
                          label="Template path"
                          size="small"
                          fullWidth
                          value={refCheatTemplatePath}
                          onChange={(e) => setRefCheatTemplatePath(e.target.value)}
                          placeholder="D:\\path\\to\\template.typ"
                        />
                        <Button
                          variant="outlined"
                          size="small"
                          disabled={pluginActionBusy || !refCheatTemplatePath.trim()}
                          onClick={() => runRefCheatAction('import_template', {
                            path: refCheatTemplatePath.trim(),
                            make_default: refCheatTemplates().length === 0,
                          })}
                        >
                          Import
                        </Button>
                      </Stack>
                      <Stack spacing={1} sx={{ mt: 1 }}>
                        {refCheatTemplates().map((template) => {
                          const isDefault = template.path === refCheatDefaultTemplate();
                          return (
                            <Stack
                              key={template.path}
                              direction={{ xs: 'column', sm: 'row' }}
                              alignItems={{ xs: 'stretch', sm: 'center' }}
                              spacing={1}
                              sx={{ borderTop: 1, borderColor: 'divider', pt: 1 }}
                            >
                              <Box sx={{ flex: 1, minWidth: 0 }}>
                                <Typography variant="body2" fontWeight={500}>{template.name}</Typography>
                                <Typography variant="caption" color="text.secondary" sx={{ wordBreak: 'break-all' }}>
                                  {template.path}
                                </Typography>
                              </Box>
                              {isDefault && <Chip label="Default" size="small" color="primary" />}
                              {template.calibrated_at && <Chip label="Calibrated" size="small" color="success" />}
                              <Button
                                variant="outlined"
                                size="small"
                                disabled={pluginActionBusy || isDefault}
                                onClick={() => runRefCheatAction('set_default_template', { template: template.name })}
                              >
                                Default
                              </Button>
                              <Button
                                variant="outlined"
                                size="small"
                                disabled={pluginActionBusy}
                                onClick={() => runRefCheatAction('calibrate_template', { template: template.name })}
                              >
                                Calibrate
                              </Button>
                              <Button
                                variant="outlined"
                                size="small"
                                color="error"
                                disabled={pluginActionBusy}
                                onClick={() => runRefCheatAction('delete_template', { template: template.name })}
                              >
                                Delete
                              </Button>
                            </Stack>
                          );
                        })}
                      </Stack>
                    </Box>
                  )}
                </Box>
              ))}
            </Stack>
          </CardContent>
          <CardActions sx={{ px: 2, pb: 2 }}>
            <Button variant="outlined" size="small" onClick={loadPlugins} disabled={saving}>
              Refresh Plugins
            </Button>
          </CardActions>
        </Card>

        {/* jAccount */}
        <Card>
          <CardContent>
            <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 2 }}>
              <AccountCircleIcon color="primary" />
              <Typography variant="h6" sx={{ fontSize: '1.1rem' }}>jAccount</Typography>
            </Stack>
            <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
              A saved jAccount cookie is used as a fallback when the Canvas cookie field
              is left empty. This is used for Canvas video subtitle fetching.
            </Typography>
            <Box>
              <Stack direction="row" alignItems="center" spacing={1} sx={{ mb: 0.5 }}>
                <Typography variant="body2" fontWeight={500}>JAAuthCookie</Typography>
                <SecretBadge set={status.jaccount.cookie_set} masked={status.jaccount.cookie_masked} />
              </Stack>
              <TextField
                type="password"
                size="small"
                fullWidth
                value={jaccountCookie}
                onChange={(e) => setJaccountCookie(e.target.value)}
                placeholder="Leave blank to keep existing cookie"
              />
            </Box>
          </CardContent>
          <CardActions sx={{ px: 2, pb: 2 }}>
            <Button variant="contained" size="small" onClick={() => save()} disabled={saving}>
              Save jAccount
            </Button>
            <Button variant="outlined" size="small" onClick={() => clearField('jaccount_cookie')} disabled={saving} color="error">
              Clear Cookie
            </Button>
          </CardActions>
        </Card>
      </Stack>

      <Typography variant="body2" color="text.secondary" sx={{ mt: 3 }}>
        Credentials are stored in the selected project directory as{' '}
        <code>secrets.local.json</code>. Do not share that file.
      </Typography>
    </Box>
  );
}
