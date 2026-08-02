import React, { useEffect, useMemo, useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import {
  Box,
  Typography,
  Card,
  CardContent,
  FormControl,
  FormLabel,
  RadioGroup,
  FormControlLabel,
  Radio,
  Button,
  Alert,
  Grid,
  Checkbox,
} from '@mui/material'
import { useSnackbar } from 'notistack'
import { useTaskStore } from '../stores/taskStore'
import { TaskStatus, ResolutionPolicy } from '../types'
import {
  cleanupPictures,
  cleanupOutdatedAvatars,
  cleanupInvalidPosts,
  cleanupInvalidPictures,
  inspectLegacySource,
  createUserBackup,
  listUserBackups,
  restoreUserBackup,
  UserBackup,
  verifyUserBackup,
} from '../lib/api'
import LegacyImportDialog from '../components/LegacyImportDialog'
import { LegacyDetection } from '../types/legacy'

const DataManage: React.FC = () => {
  const { enqueueSnackbar } = useSnackbar()
  const isTaskRunning = useTaskStore(state => state.currentTask?.status === TaskStatus.InProgress)
  const fetchCurrentTask = useTaskStore(state => state.fetchCurrentTask)

  const [policy, setPolicy] = useState<ResolutionPolicy>(ResolutionPolicy.Highest)
  const [cleanRetweetedInvalid, setCleanRetweetedInvalid] = useState(false)
  const [legacyDetection, setLegacyDetection] = useState<LegacyDetection | null>(null)
  const [legacyDialogOpen, setLegacyDialogOpen] = useState(false)
  const [selectingLegacyFile, setSelectingLegacyFile] = useState(false)
  const [userBackups, setUserBackups] = useState<UserBackup[]>([])
  const [backupBusy, setBackupBusy] = useState(false)
  const legacyDetections = useMemo(
    () => (legacyDetection ? [legacyDetection] : []),
    [legacyDetection]
  )

  const refreshUserBackups = async () => {
    try {
      setUserBackups(await listUserBackups())
    } catch {
      enqueueSnackbar('无法读取用户数据备份列表', { variant: 'error' })
    }
  }

  useEffect(() => {
    void listUserBackups()
      .then(setUserBackups)
      .catch(() => enqueueSnackbar('无法读取用户数据备份列表', { variant: 'error' }))
  }, [enqueueSnackbar])

  const handleCreateUserBackup = async () => {
    setBackupBusy(true)
    try {
      const backup = await createUserBackup()
      await refreshUserBackups()
      enqueueSnackbar(`已创建备份，包含 ${backup.file_count} 个文件`, { variant: 'success' })
    } catch {
      enqueueSnackbar('创建用户数据备份失败', { variant: 'error' })
    } finally {
      setBackupBusy(false)
    }
  }

  const handleVerifyUserBackup = async (backup: UserBackup) => {
    setBackupBusy(true)
    try {
      await verifyUserBackup(backup.id)
      enqueueSnackbar('备份校验通过', { variant: 'success' })
    } catch {
      enqueueSnackbar('备份校验失败，已拒绝恢复且当前数据未改变', { variant: 'error' })
    } finally {
      setBackupBusy(false)
    }
  }

  const handleRestoreUserBackup = async (backup: UserBackup) => {
    if (!window.confirm('恢复将替换当前数据库和媒体文件。系统会先创建回滚快照，确认继续吗？')) return
    setBackupBusy(true)
    try {
      const result = await restoreUserBackup(backup.id)
      enqueueSnackbar(
        result.rollback_created
          ? '恢复完成，已创建回滚快照。应用即将退出并可重新启动。'
          : '恢复完成。应用即将退出并可重新启动。',
        { variant: 'warning', persist: true }
      )
    } catch (error) {
      const message = error instanceof Error ? error.message : ''
      enqueueSnackbar(
        message.includes('Restore blocked')
          ? '恢复被拒绝：请先取消并等待当前任务结束。'
          : message.includes('live data connection closed')
            ? '恢复未完成，应用已停止访问当前数据。请手动重新启动后再检查数据。'
          : '恢复失败，当前数据未被替换。',
        { variant: 'error', persist: message.includes('live data connection closed') }
      )
    } finally {
      setBackupBusy(false)
    }
  }

  const handleSelectLegacySnapshot = async () => {
    setSelectingLegacyFile(true)
    try {
      const selected = await open({
        multiple: false,
        title: '选择旧版 Weiback 快照',
        filters: [{ name: 'WeiBack 数据库', extensions: ['db'] }],
      })
      if (typeof selected !== 'string') return
      if (selected.split(/[\\/]/).pop()?.toLowerCase() !== 'weiback.db') {
        enqueueSnackbar('仅可选择名为 weiback.db 的旧版快照', { variant: 'warning' })
        return
      }
      const inspected = await inspectLegacySource(selected)
      setLegacyDetection(inspected)
      setLegacyDialogOpen(true)
    } catch {
      enqueueSnackbar('无法识别该旧版快照', { variant: 'error' })
    } finally {
      setSelectingLegacyFile(false)
    }
  }

  const handleCleanup = async () => {
    try {
      await cleanupPictures(policy)
      enqueueSnackbar('图片清理任务已启动', { variant: 'success' })
      fetchCurrentTask()
    } catch (e) {
      enqueueSnackbar(`启动清理任务失败: ${e}`, { variant: 'error' })
    }
  }

  const handleCleanupAvatars = async () => {
    try {
      await cleanupOutdatedAvatars()
      enqueueSnackbar('失效头像清理任务已启动', { variant: 'success' })
      fetchCurrentTask()
    } catch (e) {
      enqueueSnackbar(`启动头像清理失败: ${e}`, { variant: 'error' })
    }
  }

  const handleCleanupInvalidPosts = async () => {
    try {
      await cleanupInvalidPosts({ clean_retweeted_invalid: cleanRetweetedInvalid })
      enqueueSnackbar('失效内容清理任务已启动', { variant: 'success' })
      fetchCurrentTask()
    } catch (e) {
      enqueueSnackbar(`启动失效内容清理失败: ${e}`, { variant: 'error' })
    }
  }

  const handleCleanupInvalidPictures = async () => {
    try {
      await cleanupInvalidPictures()
      enqueueSnackbar('失效图片清理任务已启动', { variant: 'success' })
      fetchCurrentTask()
    } catch (e) {
      enqueueSnackbar(`启动失效图片清理失败: ${e}`, { variant: 'error' })
    }
  }

  return (
    <Box sx={{ p: 3 }}>
      <Typography variant="h4" gutterBottom>
        全局数据维护
      </Typography>

      <Grid container spacing={3}>
        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                用户数据备份与恢复
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                创建 SQLite 一致性快照和已下载、仍被数据库引用的媒体文件。不会备份登录会话。
              </Typography>
              <Alert severity="warning" sx={{ mb: 2 }}>
                恢复会先校验清单与文件哈希，并创建当前数据的回滚快照。恢复成功后必须重新启动应用。
              </Alert>
              <Button
                variant="contained"
                fullWidth
                onClick={() => void handleCreateUserBackup()}
                disabled={isTaskRunning || backupBusy}
                sx={{ mb: 2 }}
              >
                {backupBusy ? '处理中...' : '创建用户数据备份'}
              </Button>
              {userBackups.map(backup => (
                <Box key={backup.id} sx={{ mb: 1 }}>
                  <Typography variant="caption" display="block">
                    {new Date(backup.created_at).toLocaleString()}，{backup.file_count} 个文件
                  </Typography>
                  <Button size="small" onClick={() => void handleVerifyUserBackup(backup)} disabled={backupBusy}>
                    校验
                  </Button>
                  <Button size="small" color="warning" onClick={() => void handleRestoreUserBackup(backup)} disabled={backupBusy}>
                    恢复
                  </Button>
                </Box>
              ))}
            </CardContent>
          </Card>
        </Grid>

        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                旧版快照导入
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                从旧版 Weiback 的 `weiback.db`
                创建一次性只读导入。导入前会进行兼容性检测，原快照不会被修改。
              </Typography>
              <Alert severity="info" sx={{ mb: 2 }}>
                导入会创建回滚备份；若出现部分恢复，已导入内容可保留，待下载媒体会标记为待恢复。
              </Alert>
              <Button
                variant="contained"
                fullWidth
                onClick={() => void handleSelectLegacySnapshot()}
                disabled={isTaskRunning || selectingLegacyFile}
              >
                {selectingLegacyFile ? '正在选择并检测...' : '选择 weiback.db 并导入'}
              </Button>
            </CardContent>
          </Card>
        </Grid>

        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                图片清理 (清晰度去重)
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                如果同一张微博图片存在多种清晰度（如缩略图和原图），此操作将根据您的选择保留其中一个，并删除多余的文件及数据库记录。
              </Typography>

              <Alert severity="warning" sx={{ mb: 2 }}>
                此操作不可逆，请在执行前确认备份重要数据。
              </Alert>

              <FormControl component="fieldset">
                <FormLabel component="legend">保留策略</FormLabel>
                <RadioGroup
                  value={policy}
                  onChange={e => setPolicy(e.target.value as ResolutionPolicy)}
                >
                  <FormControlLabel
                    value={ResolutionPolicy.Highest}
                    control={<Radio />}
                    label="保留最高清晰度 (推荐)"
                  />
                  <FormControlLabel
                    value={ResolutionPolicy.Lowest}
                    control={<Radio />}
                    label="保留最低清晰度 (节省空间)"
                  />
                </RadioGroup>
              </FormControl>

              <Box sx={{ mt: 3 }}>
                <Button
                  variant="contained"
                  color="primary"
                  fullWidth
                  onClick={handleCleanup}
                  disabled={isTaskRunning}
                >
                  {isTaskRunning ? '任务进行中...' : '开始清理'}
                </Button>
              </Box>
            </CardContent>
          </Card>
        </Grid>

        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                头像清理 (失效去重)
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                微博用户更换头像后，本地可能仍保留着旧的头像文件。此操作将对比数据库中记录的最新头像，清理所有已失效的历史头像文件。
              </Typography>

              <Alert severity="info" sx={{ mb: 2 }}>
                仅清理 user 表中已记录的用户的历史头像。
              </Alert>

              <Box sx={{ mt: 3 }}>
                <Button
                  variant="contained"
                  color="primary"
                  fullWidth
                  onClick={handleCleanupAvatars}
                  disabled={isTaskRunning}
                >
                  {isTaskRunning ? '任务进行中...' : '开始清理失效头像'}
                </Button>
              </Box>
            </CardContent>
          </Card>
        </Grid>

        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                失效微博清理
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                清理数据库中的失效微博。这些内容通常由于原作者注销或删除或者不可抗力而无法正常显示。
              </Typography>

              <Alert severity="warning" sx={{ mb: 2 }}>
                此操作将永久删除失效微博及其关联媒体。
              </Alert>

              <Box sx={{ mb: 2 }}>
                <FormControlLabel
                  control={
                    <Checkbox
                      checked={cleanRetweetedInvalid}
                      onChange={e => setCleanRetweetedInvalid(e.target.checked)}
                    />
                  }
                  label={<strong>深度清理模式</strong>}
                />
                <Typography variant="caption" display="block" color="text.secondary" sx={{ ml: 4 }}>
                  {cleanRetweetedInvalid ? (
                    <span>
                      <strong>当前模式：</strong>{' '}
                      清理所有失效内容。同时，如果某条正常微博转发的内容已失效，该正常微博也将被一并删除（保持数据库绝对纯净）。
                    </span>
                  ) : (
                    <span>
                      <strong>当前模式（默认）：</strong>{' '}
                      仅清理“独立”的失效内容。如果某条失效微博被你保存的其他微博转发了，为了保持转发链条的完整性，将予以保留。
                    </span>
                  )}
                </Typography>
              </Box>

              <Box sx={{ mt: 3 }}>
                <Button
                  variant="contained"
                  color="primary"
                  fullWidth
                  onClick={handleCleanupInvalidPosts}
                  disabled={isTaskRunning}
                >
                  {isTaskRunning ? '任务进行中...' : '开始清理失效内容'}
                </Button>
              </Box>
            </CardContent>
          </Card>
        </Grid>

        <Grid size={{ xs: 12, md: 6 }}>
          <Card>
            <CardContent>
              <Typography variant="h6" gutterBottom>
                失效图片清理
              </Typography>
              <Typography variant="body2" color="text.secondary" sx={{ mb: 2 }}>
                清理本地存储中已失效的图片: 1. 下载出错导致无法正常解析的图片; 2.
                被和谐导致大眼化的图片。
              </Typography>

              <Alert severity="warning" sx={{ mb: 2 }}>
                此操作将永久删除失效图片文件及其数据库记录。
              </Alert>

              <Box sx={{ mt: 3 }}>
                <Button
                  variant="contained"
                  color="primary"
                  fullWidth
                  onClick={handleCleanupInvalidPictures}
                  disabled={isTaskRunning}
                >
                  {isTaskRunning ? '任务进行中...' : '开始清理失效图片'}
                </Button>
              </Box>
            </CardContent>
          </Card>
        </Grid>
      </Grid>
      <LegacyImportDialog
        open={legacyDialogOpen}
        detections={legacyDetections}
        onCancel={() => setLegacyDialogOpen(false)}
        onCompleted={() => {
          setLegacyDialogOpen(false)
          setLegacyDetection(null)
          void fetchCurrentTask()
          enqueueSnackbar('旧版快照导入结果已更新', { variant: 'success' })
        }}
      />
    </Box>
  )
}

export default DataManage
