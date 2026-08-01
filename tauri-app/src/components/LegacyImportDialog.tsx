import React from 'react'
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
  Alert,
} from '@mui/material'
import { LegacyDetection } from '../types/legacy'

const kindLabel = (kind: LegacyDetection['kind']) =>
  kind === 'python_v2' ? 'Python v2 采集器' : '旧版 Rust 采集器'

interface LegacyImportDialogProps {
  open: boolean
  detections: LegacyDetection[]
  onClose: () => void
}

const LegacyImportDialog: React.FC<LegacyImportDialogProps> = ({ open, detections, onClose }) => {
  return (
    <Dialog open={open} onClose={onClose} maxWidth="sm" fullWidth>
      <DialogTitle>检测到旧版数据</DialogTitle>
      <DialogContent>
        <DialogContentText sx={{ mb: 1 }}>
          检测到系统中存在旧版 WeiBack 的数据文件。新版以独立身份运行，不会自动读取或修改旧版数据，
          你可以稍后通过导入功能把旧版数据一次性迁移过来。
        </DialogContentText>
        <List dense>
          {detections.map(detection => (
            <ListItem key={detection.db_path} disableGutters>
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
        <Alert severity="info" sx={{ mb: 1 }}>
          导入是一次性只读快照，旧 session 不会被迁移，也不会影响旧版继续使用。
        </Alert>
        <Alert severity="warning">
          注意：请避免在旧版与新版的自动同步同时开启的情况下运行，以免同一账号被频繁请求导致限流。
        </Alert>
      </DialogContent>
      <DialogActions>
        <Button onClick={onClose} color="primary">
          知道了
        </Button>
        <Button onClick={onClose} disabled>
          导入旧版数据（即将开放）
        </Button>
      </DialogActions>
    </Dialog>
  )
}

export default LegacyImportDialog
