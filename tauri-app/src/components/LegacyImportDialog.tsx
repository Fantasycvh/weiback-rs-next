import React, { useEffect, useState } from 'react'
import {
  Dialog,
  DialogTitle,
  DialogContent,
  DialogContentText,
  DialogActions,
  Button,
  List,
  ListItem,
  ListItemText,
  Chip,
  Box,
  Alert,
  CircularProgress,
  FormControlLabel,
  Radio,
  RadioGroup,
  Stack,
  Typography,
} from '@mui/material'
import { importLegacySource, inspectLegacySource } from '../lib/api'
import { LegacyDetection, LegacyImportSummary } from '../types/legacy'

const kindLabel = (kind: LegacyDetection['kind']) =>
  kind === 'python_v2' ? 'Python v2 采集器' : '旧版 Rust 采集器'

interface LegacyImportDialogProps {
  open: boolean
  detections: LegacyDetection[]
  onCancel: () => void
  onCompleted: () => void
}

const statusContent = (summary: LegacyImportSummary) => {
  if (summary.status === 'completed') return { severity: 'success' as const, title: '导入完成' }
  if (summary.status === 'already_completed')
    return { severity: 'info' as const, title: '此前已完成导入' }
  return { severity: 'warning' as const, title: '已部分恢复，可继续使用' }
}

const LegacyImportDialog: React.FC<LegacyImportDialogProps> = ({
  open,
  detections,
  onCancel,
  onCompleted,
}) => {
  const [selectedPath, setSelectedPath] = useState('')
  const [inspecting, setInspecting] = useState(false)
  const [importing, setImporting] = useState(false)
  const [inspected, setInspected] = useState<LegacyDetection | null>(null)
  const [summary, setSummary] = useState<LegacyImportSummary | null>(null)
  const [importError, setImportError] = useState(false)

  useEffect(() => {
    if (!open) return
    setSelectedPath(detections[0]?.db_path || '')
    setInspecting(false)
    setImporting(false)
    setInspected(null)
    setSummary(null)
    setImportError(false)
  }, [detections, open])

  const busy = inspecting || importing
  const selectedDetection = detections.find(item => item.db_path === selectedPath)

  const handleInspect = async () => {
    if (!selectedPath) return
    setInspecting(true)
    setInspected(null)
    setImportError(false)
    try {
      setInspected(await inspectLegacySource(selectedPath))
    } catch {
      setInspected(null)
    } finally {
      setInspecting(false)
    }
  }

  const handleImport = async () => {
    if (!inspected || importing) return
    setImporting(true)
    setImportError(false)
    try {
      setSummary(await importLegacySource(inspected.db_path))
    } catch {
      // The backend intentionally exposes only a stable failure category.
      setSummary(null)
      setImportError(true)
    } finally {
      setImporting(false)
    }
  }

  const closeAfterSuccess = () => {
    if (summary) onCompleted()
  }

  return (
    <Dialog open={open} onClose={busy || summary ? undefined : onCancel} maxWidth="sm" fullWidth>
      <DialogTitle>检测到旧版数据</DialogTitle>
      <DialogContent>
        <DialogContentText sx={{ mb: 1 }}>
          检测到系统中存在旧版 WeiBack 的数据文件。新版以独立身份运行，不会自动读取或修改旧版数据，
          你可以稍后通过导入功能把旧版数据一次性迁移过来。
        </DialogContentText>
        <RadioGroup
          value={selectedPath}
          onChange={event => {
            setSelectedPath(event.target.value)
            setInspected(null)
          }}
        >
          <List dense>
            {detections.map(detection => (
              <ListItem key={detection.db_path} disableGutters>
                <FormControlLabel
                  value={detection.db_path}
                  control={<Radio disabled={busy || !!summary} />}
                  label=""
                  sx={{ mr: 0 }}
                />
                <ListItemText
                  primary={kindLabel(detection.kind)}
                  secondary={detection.db_path}
                  secondaryTypographyProps={{ sx: { wordBreak: 'break-all' } }}
                />
                <Chip
                  label={`schema v${detection.schema_version}`}
                  size="small"
                  variant="outlined"
                  sx={{ ml: 1 }}
                />
              </ListItem>
            ))}
          </List>
        </RadioGroup>
        <Alert severity="info" sx={{ mb: 1 }}>
          导入是一次性只读快照，旧 session 不会被迁移，也不会影响旧版继续使用。
        </Alert>
        <Alert severity="warning">
          注意：请避免在旧版与新版的自动同步同时开启的情况下运行，以免同一账号被频繁请求导致限流。
        </Alert>
        {!summary && selectedPath && (
          <Box sx={{ mt: 2 }}>
            <Button variant="outlined" onClick={() => void handleInspect()} disabled={busy}>
              {inspecting ? <CircularProgress size={18} /> : '检测所选快照'}
            </Button>
            {inspected && (
              <Alert severity="success" sx={{ mt: 1 }}>
                已确认 {kindLabel(inspected.kind)}，可执行只读导入。
              </Alert>
            )}
            {!inspecting && !inspected && selectedDetection && (
              <Alert severity="info" sx={{ mt: 1 }}>
                请选择并检测快照后再导入。
              </Alert>
            )}
          </Box>
        )}
        {summary &&
          (() => {
            const content = statusContent(summary)
            return (
              <Alert severity={content.severity} sx={{ mt: 2 }}>
                <Typography fontWeight={600}>{content.title}</Typography>
                <Typography variant="body2">
                  已处理微博 {summary.posts} 条、用户 {summary.users} 个；媒体已复制{' '}
                  {summary.media_copied} 个，待下载 {summary.media_pending} 个。
                </Typography>
                <Typography variant="body2">回滚备份：{summary.rollback_backup}</Typography>
              </Alert>
            )
          })()}
        {importError && (
          <Alert severity="error" sx={{ mt: 2 }}>
            导入未完成，请稍后重试。
          </Alert>
        )}
      </DialogContent>
      <DialogActions>
        {summary ? (
          <Button onClick={closeAfterSuccess}>完成</Button>
        ) : (
          <>
            <Button onClick={onCancel} disabled={busy}>
              取消
            </Button>
            <Button
              variant="contained"
              onClick={() => void handleImport()}
              disabled={!inspected || busy}
            >
              {importing ? (
                <Stack direction="row" spacing={1} alignItems="center">
                  <CircularProgress size={18} color="inherit" />
                  <span>正在导入</span>
                </Stack>
              ) : (
                '导入旧版数据'
              )}
            </Button>
          </>
        )}
      </DialogActions>
    </Dialog>
  )
}

export default LegacyImportDialog
