import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Alert,
  Box,
  Button,
  Card,
  CardContent,
  Chip,
  CircularProgress,
  Divider,
  FormControlLabel,
  Grid,
  IconButton,
  LinearProgress,
  MenuItem,
  Stack,
  Switch,
  Tab,
  Tabs,
  TextField,
  Tooltip,
  Typography,
} from '@mui/material'
import CancelIcon from '@mui/icons-material/Cancel'
import DeleteIcon from '@mui/icons-material/Delete'
import EditIcon from '@mui/icons-material/Edit'
import PauseIcon from '@mui/icons-material/Pause'
import PlayArrowIcon from '@mui/icons-material/PlayArrow'
import RefreshIcon from '@mui/icons-material/Refresh'
import ReplayIcon from '@mui/icons-material/Replay'
import SyncIcon from '@mui/icons-material/Sync'
import { useSnackbar } from 'notistack'
import {
  cancelSyncJob,
  deleteMonitoredUser,
  deleteSyncAccount,
  enqueueSyncJob,
  getMonitoredUsers,
  getSyncAccounts,
  getSyncDiagnostics,
  getSyncJobs,
  getSyncRunHistory,
  pauseSyncJob,
  resumeSyncJob,
  retrySyncJob,
  saveMonitoredUser,
  saveSyncAccount,
} from '../lib/api'
import { formatEpoch, getRunCounters, parseRunStats, redactDiagnostic } from '../lib/p3Utils'
import { useVisiblePolling } from '../hooks/useVisiblePolling'
import type {
  MonitoredUser,
  RefreshTier,
  SyncAccount,
  SyncDiagnostics,
  SyncJob,
  SyncRun,
} from '../types'

const POLL_INTERVAL = 15_000
const DEFAULT_INTERVALS: Record<RefreshTier, number> = { hot: 900, warm: 3600, cold: 21600 }

type AccountDraft = {
  id?: string
  provider: string
  uid: string
  displayName: string
  sessionRef: string
  enabled: boolean
  replaceSession: boolean
}

type MonitorDraft = {
  accountId: string
  uid: string
  screenName: string
  tier: RefreshTier
  intervalSecs: string
  jitterSecs: string
  enabled: boolean
}

const blankAccount = (): AccountDraft => ({
  provider: 'weibo', uid: '', displayName: '', sessionRef: '', enabled: true, replaceSession: false,
})

const blankMonitor = (accountId = ''): MonitorDraft => ({
  accountId, uid: '', screenName: '', tier: 'warm', intervalSecs: '3600', jitterSecs: '300', enabled: true,
})

const statusColor = (status: string): 'default' | 'success' | 'warning' | 'error' | 'info' => {
  switch (status.toLowerCase()) {
    case 'completed': return 'success'
    case 'failed': return 'error'
    case 'running': return 'info'
    case 'paused': return 'warning'
    default: return 'default'
  }
}

const DiagnosticBlock = ({ title, value }: { title: string; value: unknown }) => (
  <Card variant="outlined" sx={{ height: '100%', borderRadius: 1 }}>
    <CardContent>
      <Typography variant="subtitle1" fontWeight={500} mb={1}>{title}</Typography>
      <Box component="pre" sx={{ m: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', fontSize: 12 }}>
        {JSON.stringify(redactDiagnostic(value ?? { status: '不可用' }), null, 2)}
      </Box>
    </CardContent>
  </Card>
)

const SyncCenter = () => {
  const { enqueueSnackbar } = useSnackbar()
  const [accounts, setAccounts] = useState<SyncAccount[]>([])
  const [monitors, setMonitors] = useState<MonitoredUser[]>([])
  const [jobs, setJobs] = useState<SyncJob[]>([])
  const [diagnostics, setDiagnostics] = useState<SyncDiagnostics | null>(null)
  const [accountDraft, setAccountDraft] = useState<AccountDraft>(blankAccount)
  const [monitorDraft, setMonitorDraft] = useState<MonitorDraft>(blankMonitor)
  const [selectedJobId, setSelectedJobId] = useState<string>('')
  const [runs, setRuns] = useState<SyncRun[]>([])
  const [loading, setLoading] = useState(true)
  const [busyKey, setBusyKey] = useState<string | null>(null)
  const [tab, setTab] = useState(0)
  const mountedRef = useRef(false)
  const refreshGenerationRef = useRef(0)
  const usableAccounts = accounts.filter(account => account.enabled && account.session_ready)

  useEffect(() => {
    mountedRef.current = true
    return () => {
      mountedRef.current = false
      refreshGenerationRef.current += 1
    }
  }, [])

  const refresh = useCallback(async (showError = false) => {
    const requestGeneration = ++refreshGenerationRef.current
    const isCurrent = () => mountedRef.current && requestGeneration === refreshGenerationRef.current
    try {
      const [nextAccounts, nextMonitors, nextJobs, nextDiagnostics] = await Promise.all([
        getSyncAccounts(), getMonitoredUsers(), getSyncJobs(), getSyncDiagnostics(),
      ])
      if (!isCurrent()) return
      setAccounts(nextAccounts)
      setMonitors(nextMonitors)
      setJobs(nextJobs)
      setDiagnostics(nextDiagnostics)
      const nextUsableAccount = nextAccounts.find(account => account.enabled && account.session_ready)
      setMonitorDraft(current => {
        const currentIsUsable = nextAccounts.some(account => account.id === current.accountId && account.enabled && account.session_ready)
        return currentIsUsable || !nextUsableAccount
          ? current
          : { ...current, accountId: nextUsableAccount.id }
      })
      setSelectedJobId(current => nextJobs.some(job => job.id === current)
        ? current
        : nextJobs[0]?.id || '')
    } catch (error) {
      if (isCurrent() && showError) enqueueSnackbar(`刷新同步状态失败: ${error}`, { variant: 'error' })
    } finally {
      if (isCurrent()) setLoading(false)
    }
  }, [enqueueSnackbar])

  useEffect(() => { void refresh(true) }, [refresh])
  useVisiblePolling(refresh, POLL_INTERVAL)

  useEffect(() => {
    if (!selectedJobId) { setRuns([]); return }
    let active = true
    getSyncRunHistory(selectedJobId).then(value => { if (active) setRuns(value) }).catch(error => {
      if (active) enqueueSnackbar(`读取运行历史失败: ${error}`, { variant: 'error' })
    })
    return () => { active = false }
  }, [enqueueSnackbar, jobs, selectedJobId])

  const runAction = async (key: string, action: () => Promise<unknown>, message: string) => {
    setBusyKey(key)
    try {
      await action()
      if (!mountedRef.current) return
      enqueueSnackbar(message, { variant: 'success' })
      await refresh(true)
    } catch (error) {
      if (mountedRef.current) enqueueSnackbar(`操作失败: ${error}`, { variant: 'error' })
    } finally {
      if (mountedRef.current) setBusyKey(null)
    }
  }

  const submitAccount = async () => {
    if (!accountDraft.id) {
      enqueueSnackbar('请先登录微博，系统会自动创建可用的同步账号', { variant: 'info' })
      return
    }
    const sessionRef = !accountDraft.id || accountDraft.replaceSession
      ? accountDraft.sessionRef.trim()
      : undefined
    await runAction('account-save', () => saveSyncAccount({
      id: accountDraft.id,
      provider: accountDraft.provider.trim(),
      uid: accountDraft.uid.trim(),
      display_name: accountDraft.displayName.trim() || null,
      enabled: accountDraft.enabled,
      ...(sessionRef ? { session_ref: sessionRef } : {}),
    }), accountDraft.id ? '账号已更新' : '账号已创建')
    setAccountDraft(blankAccount())
  }

  const editAccount = (account: SyncAccount) => setAccountDraft({
    id: account.id, provider: account.provider, uid: account.uid,
    displayName: account.display_name || '', sessionRef: '', enabled: account.enabled, replaceSession: false,
  })

  const submitMonitor = async () => {
    const intervalSecs = Number(monitorDraft.intervalSecs)
    const jitterSecs = Number(monitorDraft.jitterSecs)
    if (!monitorDraft.accountId || !monitorDraft.uid || !Number.isInteger(intervalSecs) ||
      intervalSecs < 1 || !Number.isInteger(jitterSecs) || jitterSecs < 0 || jitterSecs > intervalSecs) {
      enqueueSnackbar('请检查账号、UID、刷新间隔和抖动值', { variant: 'warning' })
      return
    }
    await runAction('monitor-save', () => saveMonitoredUser({
      account_id: monitorDraft.accountId,
      uid: monitorDraft.uid,
      screen_name: monitorDraft.screenName.trim() || null,
      refresh_strategy: monitorDraft.tier,
      tier: monitorDraft.tier,
      interval_secs: intervalSecs,
      jitter_secs: jitterSecs,
      enabled: monitorDraft.enabled,
    }), '监控用户已保存')
    setMonitorDraft(blankMonitor(monitorDraft.accountId))
  }

  const editMonitor = (monitor: MonitoredUser) => setMonitorDraft({
    accountId: monitor.account_id, uid: monitor.uid, screenName: monitor.screen_name || '',
    tier: monitor.tier, intervalSecs: monitor.interval_secs, jitterSecs: monitor.jitter_secs,
    enabled: monitor.enabled,
  })

  const collectPosts = (monitor: MonitoredUser) => runAction(
    `collect-${monitor.account_id}-${monitor.uid}`,
    () => enqueueSyncJob({ kind: 'collect_user_posts', account_id: monitor.account_id, uid: monitor.uid, max_pages: null, priority: 0 }),
    '采集任务已入队'
  )

  const latestStats = parseRunStats(runs[0]?.stats_json || null)
  const runCounters = getRunCounters(latestStats)
  const hasRunCounters = runCounters.fetchedCount !== null || runCounters.pages !== null
  const selectedJob = jobs.find(job => job.id === selectedJobId)
  const showUnknownProgress = hasRunCounters ||
    (selectedJob !== undefined && !['completed', 'failed', 'cancelled'].includes(selectedJob.status.toLowerCase()))
  const redactedDiagnostics = redactDiagnostic(diagnostics)

  if (loading) return <Box sx={{ display: 'grid', placeItems: 'center', minHeight: 320 }}><CircularProgress /></Box>

  return (
    <Box sx={{ width: '100%', maxWidth: 1440, mx: 'auto' }}>
      <Stack direction={{ xs: 'column', sm: 'row' }} justifyContent="space-between" alignItems={{ sm: 'center' }} gap={1} mb={2}>
        <Box>
          <Typography variant="h4">同步中心</Typography>
          <Typography color="text.secondary">持久任务、刷新策略与运行诊断</Typography>
        </Box>
        <Tooltip title="立即刷新"><IconButton onClick={() => void refresh(true)} aria-label="立即刷新"><RefreshIcon /></IconButton></Tooltip>
      </Stack>

      <Tabs value={tab} onChange={(_, value: number) => setTab(value)} variant="scrollable" allowScrollButtonsMobile sx={{ mb: 2 }}>
        <Tab label="账号与监控" /><Tab label="任务与历史" /><Tab label="诊断" />
      </Tabs>

      {tab === 0 && <Grid container spacing={2}>
        <Grid size={{ xs: 12, lg: 5 }}>
          <Card sx={{ borderRadius: 1 }}><CardContent>
            <Stack direction="row" alignItems="center" justifyContent="space-between" mb={2}>
              <Typography variant="h6">同步账号</Typography>
            </Stack>
            <Stack spacing={2}>
              {!accountDraft.id && <Alert severity="info">登录成功后，当前会话会自动注册为同步账号。</Alert>}
              {accountDraft.id && <TextField label="Provider" value={accountDraft.provider} disabled />}
              {accountDraft.id && <TextField label="UID" value={accountDraft.uid} disabled />}
              <TextField label="显示名称" value={accountDraft.displayName} onChange={e => setAccountDraft(v => ({ ...v, displayName: e.target.value }))} />
              {accountDraft.id && <FormControlLabel control={<Switch checked={accountDraft.enabled} disabled={!accountDraft.enabled && !accounts.find(account => account.id === accountDraft.id)?.session_ready} onChange={e => setAccountDraft(v => ({ ...v, enabled: e.target.checked }))} />} label="启用账号" />}
              {accountDraft.id && <Button variant="contained" onClick={() => void submitAccount()} disabled={busyKey === 'account-save'}>保存账号</Button>}
            </Stack>
          </CardContent></Card>
        </Grid>
        <Grid size={{ xs: 12, lg: 7 }}>
          <Stack spacing={1.5}>{accounts.length === 0 && <Alert severity="info">尚未创建同步账号。</Alert>}{accounts.map(account => <Card key={account.id} variant="outlined" sx={{ borderRadius: 1 }}><CardContent>
            <Stack direction="row" justifyContent="space-between" alignItems="flex-start" gap={1}>
              <Box sx={{ minWidth: 0 }}><Typography fontWeight={500}>{account.display_name || account.uid}</Typography><Typography variant="body2" color="text.secondary" sx={{ overflowWrap: 'anywhere' }}>{account.provider} · {account.uid} · {account.session_ready ? '会话可用' : account.has_session ? '会话不可用，请重新登录' : '无会话'}</Typography></Box>
              <Stack direction="row"><Chip size="small" label={account.enabled ? '启用' : '停用'} color={account.enabled ? 'success' : 'default'} /><Tooltip title="编辑"><IconButton size="small" onClick={() => editAccount(account)}><EditIcon /></IconButton></Tooltip><Tooltip title="删除"><IconButton size="small" color="error" onClick={() => void runAction(`delete-account-${account.id}`, () => deleteSyncAccount(account.id), '账号已删除')}><DeleteIcon /></IconButton></Tooltip></Stack>
            </Stack>
          </CardContent></Card>)}</Stack>
        </Grid>

        <Grid size={{ xs: 12, lg: 5 }}>
          <Card sx={{ borderRadius: 1 }}><CardContent><Typography variant="h6" mb={2}>监控用户</Typography><Stack spacing={2}>
            {usableAccounts.length === 0 && <Alert severity="warning">没有可用同步账号。请先登录微博或重新登录以恢复会话。</Alert>}
            <TextField select label="账号" value={monitorDraft.accountId} onChange={e => setMonitorDraft(v => ({ ...v, accountId: e.target.value }))}>{usableAccounts.map(account => <MenuItem key={account.id} value={account.id}>{account.display_name || account.uid}</MenuItem>)}</TextField>
            <TextField label="用户 UID" value={monitorDraft.uid} onChange={e => setMonitorDraft(v => ({ ...v, uid: e.target.value }))} />
            <TextField label="用户名称" value={monitorDraft.screenName} onChange={e => setMonitorDraft(v => ({ ...v, screenName: e.target.value }))} />
            <TextField select label="刷新层级" value={monitorDraft.tier} onChange={e => { const tier = e.target.value as RefreshTier; setMonitorDraft(v => ({ ...v, tier, intervalSecs: String(DEFAULT_INTERVALS[tier]) })) }}>{(['hot', 'warm', 'cold'] as RefreshTier[]).map(tier => <MenuItem key={tier} value={tier}>{tier}</MenuItem>)}</TextField>
            <Stack direction={{ xs: 'column', sm: 'row' }} spacing={2}><TextField fullWidth label="间隔（秒）" type="number" value={monitorDraft.intervalSecs} onChange={e => setMonitorDraft(v => ({ ...v, intervalSecs: e.target.value }))} /><TextField fullWidth label="抖动（秒）" type="number" value={monitorDraft.jitterSecs} onChange={e => setMonitorDraft(v => ({ ...v, jitterSecs: e.target.value }))} /></Stack>
            <FormControlLabel control={<Switch checked={monitorDraft.enabled} onChange={e => setMonitorDraft(v => ({ ...v, enabled: e.target.checked }))} />} label="启用监控" />
            <Button variant="contained" onClick={() => void submitMonitor()} disabled={!usableAccounts.length || busyKey === 'monitor-save'}>保存监控</Button>
          </Stack></CardContent></Card>
        </Grid>
        <Grid size={{ xs: 12, lg: 7 }}><Stack spacing={1.5}>{monitors.length === 0 && <Alert severity="info">尚未配置监控用户。</Alert>}{monitors.map(monitor => <Card key={`${monitor.account_id}-${monitor.uid}`} variant="outlined" sx={{ borderRadius: 1 }}><CardContent>
          <Stack direction={{ xs: 'column', sm: 'row' }} justifyContent="space-between" gap={1}><Box><Typography fontWeight={500}>{monitor.screen_name || monitor.uid}</Typography><Typography variant="body2" color="text.secondary">{monitor.tier} · 每 {monitor.interval_secs}s ± {monitor.jitter_secs}s</Typography><Typography variant="caption" color="text.secondary">下次刷新：{formatEpoch(monitor.next_refresh_epoch)}</Typography></Box><Stack direction="row" alignItems="center"><Tooltip title="立即采集帖子"><span><IconButton color="primary" disabled={!usableAccounts.some(account => account.id === monitor.account_id)} onClick={() => void collectPosts(monitor)}><SyncIcon /></IconButton></span></Tooltip><Tooltip title="编辑"><IconButton onClick={() => editMonitor(monitor)}><EditIcon /></IconButton></Tooltip><Tooltip title="删除"><IconButton color="error" onClick={() => void runAction(`delete-monitor-${monitor.account_id}-${monitor.uid}`, () => deleteMonitoredUser(monitor.account_id, monitor.uid), '监控已删除')}><DeleteIcon /></IconButton></Tooltip></Stack></Stack>
        </CardContent></Card>)}</Stack></Grid>
      </Grid>}

      {tab === 1 && <Grid container spacing={2}>
        <Grid size={{ xs: 12, lg: 7 }}><Stack spacing={1.5}>{jobs.length === 0 && <Alert severity="info">暂无持久同步任务。</Alert>}{jobs.map(job => <Card key={job.id} variant="outlined" onClick={() => setSelectedJobId(job.id)} sx={{ borderRadius: 1, cursor: 'pointer', borderColor: selectedJobId === job.id ? 'primary.main' : 'divider' }}><CardContent>
          <Stack direction={{ xs: 'column', sm: 'row' }} justifyContent="space-between" gap={1}><Box sx={{ minWidth: 0 }}><Stack direction="row" spacing={1} alignItems="center"><Typography fontWeight={500} sx={{ overflowWrap: 'anywhere' }}>{job.name}</Typography><Chip size="small" label={job.status} color={statusColor(job.status)} /></Stack><Typography variant="caption" color="text.secondary">#{job.id} · {job.kind} · 优先级 {job.priority}</Typography></Box><Stack direction="row" alignItems="center">
            <Tooltip title="暂停"><span><IconButton disabled={!['pending','running'].includes(job.status) || busyKey === job.id} onClick={e => { e.stopPropagation(); void runAction(job.id, () => pauseSyncJob(job.id), '任务已暂停') }}><PauseIcon /></IconButton></span></Tooltip>
            <Tooltip title="继续"><span><IconButton disabled={!['paused','interrupted'].includes(job.status) || busyKey === job.id} onClick={e => { e.stopPropagation(); void runAction(job.id, () => resumeSyncJob(job.id), '任务已继续') }}><PlayArrowIcon /></IconButton></span></Tooltip>
            <Tooltip title="取消"><span><IconButton disabled={['completed','failed','cancelled'].includes(job.status) || busyKey === job.id} onClick={e => { e.stopPropagation(); void runAction(job.id, () => cancelSyncJob(job.id), '任务已取消') }}><CancelIcon /></IconButton></span></Tooltip>
            <Tooltip title="重试"><span><IconButton disabled={!['failed','cancelled','interrupted'].includes(job.status) || busyKey === job.id} onClick={e => { e.stopPropagation(); void runAction(job.id, () => retrySyncJob(job.id), '任务已重试') }}><ReplayIcon /></IconButton></span></Tooltip>
          </Stack></Stack>
        </CardContent></Card>)}</Stack></Grid>
        <Grid size={{ xs: 12, lg: 5 }}><Card sx={{ borderRadius: 1 }}><CardContent><Typography variant="h6" mb={1}>运行历史</Typography>{selectedJobId ? <>{showUnknownProgress && <Box mb={2}><LinearProgress variant="indeterminate" /><Typography variant="caption">{hasRunCounters ? <>{runCounters.fetchedCount !== null ? `已提交 ${runCounters.fetchedCount} 条` : ''}{runCounters.fetchedCount !== null && runCounters.pages !== null ? ' · ' : ''}{runCounters.pages !== null ? `${runCounters.pages} 页` : ''}</> : '进度未知'}</Typography></Box>}<Stack divider={<Divider flexItem />} spacing={1.5}>{runs.length === 0 && <Typography color="text.secondary">此任务尚无运行记录。</Typography>}{runs.map(run => <Box key={run.id}><Stack direction="row" justifyContent="space-between"><Typography fontWeight={500}>{run.status}</Typography><Typography variant="caption">第 {run.attempt} 次</Typography></Stack><Typography variant="caption" color="text.secondary">{new Date(run.started_at).toLocaleString()} {run.finished_at ? `→ ${new Date(run.finished_at).toLocaleString()}` : ''}</Typography>{run.stats_json && <Typography component="pre" variant="caption" sx={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', m: 0, mt: 0.5 }}>{JSON.stringify(redactDiagnostic(parseRunStats(run.stats_json)), null, 2)}</Typography>}</Box>)}</Stack></> : <Typography color="text.secondary">选择一个任务查看历史。</Typography>}</CardContent></Card></Grid>
      </Grid>}

      {tab === 2 && <Box>
        <Typography variant="h6" mb={2}>运行环境诊断</Typography>
        {diagnostics ? <Grid container spacing={2}>
          <Grid size={{ xs: 12, md: 6 }}><DiagnosticBlock title="Chromium / Tauri 视图" value={{ chromium: diagnostics.chromium, tauri: diagnostics.tauri }} /></Grid>
          <Grid size={{ xs: 12, md: 6 }}><DiagnosticBlock title="Sidecar 健康、版本与协议" value={diagnostics.sidecar} /></Grid>
          <Grid size={{ xs: 12, md: 4 }}><DiagnosticBlock title="认证计数" value={diagnostics.auth} /></Grid>
          <Grid size={{ xs: 12, md: 4 }}><DiagnosticBlock title="Rate gates" value={diagnostics.rate_gates} /></Grid>
          <Grid size={{ xs: 12, md: 4 }}><DiagnosticBlock title="媒体状态" value={diagnostics.media} /></Grid>
          <Grid size={{ xs: 12 }}><DiagnosticBlock title="完整脱敏摘要" value={redactedDiagnostics} /></Grid>
        </Grid> : <Alert severity="warning">诊断信息不可用。</Alert>}
      </Box>}
    </Box>
  )
}

export default SyncCenter
