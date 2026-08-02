import { useCallback, useEffect, useRef, useState } from 'react'
import { useNavigate, useParams } from 'react-router-dom'
import {
  Alert,
  Avatar,
  Box,
  Button,
  Card,
  CardContent,
  Chip,
  CircularProgress,
  Divider,
  FormControl,
  IconButton,
  InputLabel,
  LinearProgress,
  MenuItem,
  Pagination,
  Select,
  Stack,
  Tooltip,
  Typography,
} from '@mui/material'
import ArrowBackIcon from '@mui/icons-material/ArrowBack'
import ExpandMoreIcon from '@mui/icons-material/ExpandMore'
import RefreshIcon from '@mui/icons-material/Refresh'
import ReplayIcon from '@mui/icons-material/Replay'
import { useSnackbar } from 'notistack'
import FullSizeImage from '../components/FullSizeImage'
import PostDisplay from '../components/PostDisplay'
import {
  enqueueSyncJob,
  getMediaBlob,
  getOwnerMedia,
  getPostComments,
  getPostDetail,
  getSyncAccounts,
  getSyncJobs,
  retryMedia,
} from '../lib/api'
import { isTerminalJobStatus, redactDiagnostic } from '../lib/p3Utils'
import type { AttachedImage, CommentItem, OwnerMedia, Post, PostInfo, SyncAccount } from '../types'

const COMMENTS_PER_PAGE = 20
const JOB_POLL_MS = 1500

const delay = (milliseconds: number): Promise<void> =>
  new Promise(resolve => {
    window.setTimeout(resolve, milliseconds)
  })

const waitUntilVisible = async (isCurrent: () => boolean): Promise<boolean> => {
  while (document.visibilityState !== 'visible') {
    if (!isCurrent()) return false
    await delay(250)
  }
  return isCurrent()
}

type RootState = {
  open: boolean
  loading: boolean
  page: number
  items: CommentItem[]
  total: number
  error: string | null
}

const mediaColor = (
  status: OwnerMedia['status']
): 'default' | 'success' | 'warning' | 'error' | 'info' => {
  if (status === 'downloaded') return 'success'
  if (status === 'failed') return 'error'
  if (status === 'downloading') return 'info'
  if (status === 'pending') return 'warning'
  return 'default'
}

const isVideoMedia = (mediaType: string) => /video|livephoto/i.test(mediaType)

const UnifiedMediaPreview = ({ item }: { item: OwnerMedia }) => {
  const [localResult, setLocalResult] = useState<{ mediaId: string; url: string | null } | null>(null)
  const generationRef = useRef(0)
  const previewEligible = ['downloaded', 'pending', 'downloading', 'failed'].includes(item.status)
  const localLoading = previewEligible && localResult?.mediaId !== item.id
  const localUrl = previewEligible && localResult?.mediaId === item.id ? localResult.url : null

  useEffect(() => {
    const requestGeneration = ++generationRef.current
    let active = true
    let objectUrl: string | null = null

    if (!previewEligible) return () => undefined

    const loadLocalMedia = async () => {
      try {
        const media = await getMediaBlob(item.owner_type, item.owner_id, item.id)
        if (!active || requestGeneration !== generationRef.current) return
        if (!media) {
          setLocalResult({ mediaId: item.id, url: null })
          return
        }

        const bytes = Uint8Array.from(Array.from(media.bytes))
        objectUrl = URL.createObjectURL(new Blob([bytes.buffer], { type: media.content_type }))
        if (!active || requestGeneration !== generationRef.current) {
          URL.revokeObjectURL(objectUrl)
          objectUrl = null
          return
        }
        setLocalResult({ mediaId: item.id, url: objectUrl })
      } catch {
        if (active && requestGeneration === generationRef.current) {
          setLocalResult({ mediaId: item.id, url: null })
        }
      }
    }

    void loadLocalMedia()
    return () => {
      active = false
      if (objectUrl) URL.revokeObjectURL(objectUrl)
    }
  }, [item.id, item.owner_id, item.owner_type, previewEligible])
  if (localLoading) {
    return (
      <Stack direction="row" spacing={1} alignItems="center" sx={{ mt: 1 }}>
        <CircularProgress size={16} />
        <Typography variant="caption" color="text.secondary">
          正在加载本地媒体...
        </Typography>
      </Stack>
    )
  }
  if (!localUrl) {
    return (
      <Alert severity="info" sx={{ mt: 1 }}>
        {item.status === 'failed'
          ? '媒体暂不可用，可重试下载。'
          : '媒体暂不可用。远程地址只会由受控下载器访问，不会由页面直接请求。'}
      </Alert>
    )
  }

  return (
    <Box sx={{ mt: 1, maxWidth: 480 }}>
      {isVideoMedia(item.media_type) ? (
        <video
          controls
          preload="metadata"
          src={localUrl}
          onError={() => {
            URL.revokeObjectURL(localUrl)
            setLocalResult({ mediaId: item.id, url: null })
          }}
          style={{ display: 'block', width: '100%', maxHeight: 360 }}
        />
      ) : (
        <Box
          component="img"
          src={localUrl}
          alt="帖子媒体"
          onError={() => {
            URL.revokeObjectURL(localUrl)
            setLocalResult({ mediaId: item.id, url: null })
          }}
          sx={{ display: 'block', maxWidth: '100%', maxHeight: 360, objectFit: 'contain' }}
        />
      )}
    </Box>
  )
}

const waitForJob = async (jobId: string, isCurrent: () => boolean): Promise<boolean> => {
  for (;;) {
    if (!(await waitUntilVisible(isCurrent))) return false
    const job = (await getSyncJobs()).find(item => item.id === jobId)
    if (!isCurrent()) return false
    if (!job) throw new Error('任务不存在或已被清理')
    if (isTerminalJobStatus(job.status)) {
      if (job.status.toLowerCase() !== 'completed') throw new Error(`采集任务状态: ${job.status}`)
      return true
    }
    await delay(JOB_POLL_MS)
  }
}

const RetweetLevel = ({ post, depth = 0 }: { post: Post; depth?: number }) => (
  <Box sx={{ borderLeft: '3px solid', borderColor: 'divider', pl: 2, mt: 1.5 }}>
    <Stack direction="row" spacing={1} alignItems="baseline" flexWrap="wrap">
      <Typography variant="subtitle2">@{post.user?.screen_name || '未知用户'}</Typography>
      <Typography variant="caption" color="text.secondary">
        {post.created_at ? new Date(post.created_at).toLocaleString() : ''} · {post.idstr}
      </Typography>
    </Stack>
    <Typography variant="body2" sx={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', mt: 0.5 }}>
      {post.text}
    </Typography>
    {post.retweeted_status && <RetweetLevel post={post.retweeted_status} depth={depth + 1} />}
  </Box>
)

const PostDetail = () => {
  const { id = '' } = useParams()
  const navigate = useNavigate()
  const { enqueueSnackbar } = useSnackbar()
  const [detail, setDetail] = useState<PostInfo | null>(null)
  const [accounts, setAccounts] = useState<SyncAccount[]>([])
  const [selectedAccountId, setSelectedAccountId] = useState('')
  const [media, setMedia] = useState<OwnerMedia[]>([])
  const [roots, setRoots] = useState<CommentItem[]>([])
  const [rootTotal, setRootTotal] = useState(0)
  const [rootPage, setRootPage] = useState(1)
  const [rootStates, setRootStates] = useState<Record<string, RootState>>({})
  const [lightboxImage, setLightboxImage] = useState<AttachedImage | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const mountedRef = useRef(false)
  const routeGenerationRef = useRef(0)
  const baseGenerationRef = useRef(0)
  const rootsGenerationRef = useRef(0)
  const childGenerationsRef = useRef(new Map<string, number>())
  const inFlightExpansionsRef = useRef(new Map<string, Promise<void>>())

  useEffect(() => {
    const childGenerations = childGenerationsRef.current
    const inFlightExpansions = inFlightExpansionsRef.current
    mountedRef.current = true
    return () => {
      mountedRef.current = false
      routeGenerationRef.current += 1
      baseGenerationRef.current += 1
      rootsGenerationRef.current += 1
      childGenerations.clear()
      inFlightExpansions.clear()
    }
  }, [])

  useEffect(() => {
    routeGenerationRef.current += 1
    setRootPage(1)
    setRoots([])
    setRootTotal(0)
    setRootStates({})
    childGenerationsRef.current.clear()
    inFlightExpansionsRef.current.clear()
  }, [id])

  const loadBase = useCallback(async () => {
    if (!id) return
    const requestGeneration = ++baseGenerationRef.current
    const routeGeneration = routeGenerationRef.current
    const isCurrent = () =>
      mountedRef.current &&
      requestGeneration === baseGenerationRef.current &&
      routeGeneration === routeGenerationRef.current
    setLoading(true)
    setError(null)
    try {
      const [nextDetail, nextMedia, nextAccounts] = await Promise.all([
        getPostDetail(id),
        getOwnerMedia('post', id),
        getSyncAccounts(),
      ])
      if (!isCurrent()) return
      if (!nextDetail) {
        setDetail(null)
        setError('未找到该帖子')
        return
      }
      const eligibleAccounts = nextAccounts.filter(
        account => account.enabled && account.has_session
      )
      setDetail(nextDetail)
      setMedia(nextMedia)
      setAccounts(eligibleAccounts)
      setSelectedAccountId(current =>
        eligibleAccounts.some(account => account.id === current) ? current : ''
      )
    } catch (reason) {
      if (isCurrent()) setError(String(reason))
    } finally {
      if (isCurrent()) setLoading(false)
    }
  }, [id])

  const loadRoots = useCallback(
    async (page: number) => {
      if (!id) return
      const requestGeneration = ++rootsGenerationRef.current
      const routeGeneration = routeGenerationRef.current
      try {
        const result = await getPostComments(
          id,
          null,
          (page - 1) * COMMENTS_PER_PAGE,
          COMMENTS_PER_PAGE
        )
        if (
          !mountedRef.current ||
          requestGeneration !== rootsGenerationRef.current ||
          routeGeneration !== routeGenerationRef.current
        )
          return
        setRoots(result.items)
        setRootTotal(Number(result.total_items) || 0)
      } catch (reason) {
        if (
          mountedRef.current &&
          requestGeneration === rootsGenerationRef.current &&
          routeGeneration === routeGenerationRef.current
        ) {
          enqueueSnackbar(`读取评论失败: ${reason}`, { variant: 'error' })
        }
      }
    },
    [enqueueSnackbar, id]
  )

  useEffect(() => {
    void loadBase()
  }, [loadBase])
  useEffect(() => {
    void loadRoots(rootPage)
  }, [loadRoots, rootPage])

  const queryChildren = async (rootId: string, page: number, expectedGeneration?: number) => {
    const requestGeneration =
      expectedGeneration ?? (childGenerationsRef.current.get(rootId) || 0) + 1
    if (expectedGeneration === undefined) {
      childGenerationsRef.current.set(rootId, requestGeneration)
    }
    const routeGeneration = routeGenerationRef.current
    try {
      const result = await getPostComments(
        id,
        rootId,
        (page - 1) * COMMENTS_PER_PAGE,
        COMMENTS_PER_PAGE
      )
      if (
        !mountedRef.current ||
        childGenerationsRef.current.get(rootId) !== requestGeneration ||
        routeGeneration !== routeGenerationRef.current
      )
        return
      setRootStates(current => ({
        ...current,
        [rootId]: {
          open: true,
          loading: false,
          page,
          items: result.items,
          total: Number(result.total_items) || 0,
          error: null,
        },
      }))
    } catch (reason) {
      if (
        mountedRef.current &&
        childGenerationsRef.current.get(rootId) === requestGeneration &&
        routeGeneration === routeGenerationRef.current
      ) {
        setRootStates(current => ({
          ...current,
          [rootId]: {
            open: true,
            loading: false,
            page,
            items: current[rootId]?.items || [],
            total: current[rootId]?.total || 0,
            error: String(redactDiagnostic(reason instanceof Error ? reason.message : reason)),
          },
        }))
      }
      throw reason
    }
  }

  const collectAndLoad = (root: CommentItem, page = 1): Promise<void> => {
    const key = `${id}:${root.id}`
    const existing = inFlightExpansionsRef.current.get(key)
    if (existing) return existing
    const routeGeneration = routeGenerationRef.current
    const interactionGeneration = (childGenerationsRef.current.get(root.id) || 0) + 1
    childGenerationsRef.current.set(root.id, interactionGeneration)
    const isCurrent = () =>
      mountedRef.current &&
      routeGeneration === routeGenerationRef.current &&
      childGenerationsRef.current.get(root.id) === interactionGeneration
    const promise = (async () => {
      const account = accounts.find(item => item.id === selectedAccountId)
      if (!account) throw new Error('请选择一个已启用且配置会话的账号')
      if (!isCurrent()) return
      setRootStates(current => ({
        ...current,
        [root.id]: {
          open: true,
          loading: true,
          page,
          items: current[root.id]?.items || [],
          total: current[root.id]?.total || 0,
          error: null,
        },
      }))
      const jobId = await enqueueSyncJob({
        kind: 'collect_comment_replies',
        account_id: account.id,
        post_id: id,
        root_comment_id: root.id,
        max_pages: null,
        priority: 10,
      })
      if (!isCurrent() || !(await waitForJob(jobId, isCurrent))) return
      await queryChildren(root.id, page, interactionGeneration)
    })()
      .catch(reason => {
        if (!isCurrent()) return
        const message = String(redactDiagnostic(reason instanceof Error ? reason.message : reason))
        setRootStates(current => ({
          ...current,
          [root.id]: {
            open: true,
            loading: false,
            page,
            items: current[root.id]?.items || [],
            total: current[root.id]?.total || 0,
            error: message,
          },
        }))
        throw reason
      })
      .finally(() => {
        if (inFlightExpansionsRef.current.get(key) === promise) {
          inFlightExpansionsRef.current.delete(key)
        }
      })
    inFlightExpansionsRef.current.set(key, promise)
    return promise
  }

  const toggleRoot = (root: CommentItem) => {
    const state = rootStates[root.id]
    if (state?.open) {
      childGenerationsRef.current.set(root.id, (childGenerationsRef.current.get(root.id) || 0) + 1)
      inFlightExpansionsRef.current.delete(`${id}:${root.id}`)
      setRootStates(current => ({ ...current, [root.id]: { ...state, open: false } }))
      return
    }
    if (state?.items.length) {
      setRootStates(current => ({ ...current, [root.id]: { ...state, open: true } }))
      return
    }
    void collectAndLoad(root).catch(() => undefined)
  }

  const handleRetryMedia = async (item: OwnerMedia) => {
    try {
      const queued = await retryMedia(item.id)
      if (!queued) throw new Error('媒体不存在或当前状态不可重试')
      enqueueSnackbar('媒体已重新入队', { variant: 'success' })
      await loadBase()
    } catch (reason) {
      enqueueSnackbar(`媒体重试失败: ${redactDiagnostic(String(reason))}`, { variant: 'error' })
    }
  }

  if (loading)
    return (
      <Box sx={{ display: 'grid', placeItems: 'center', minHeight: 360 }}>
        <CircularProgress />
      </Box>
    )

  return (
    <Box sx={{ width: '100%', maxWidth: 1100, mx: 'auto' }}>
      <Stack direction="row" alignItems="center" spacing={1} mb={2}>
        <Tooltip title="返回内容浏览">
          <IconButton onClick={() => navigate('/explorer')} aria-label="返回内容浏览">
            <ArrowBackIcon />
          </IconButton>
        </Tooltip>
        <Typography variant="h4" sx={{ minWidth: 0, overflowWrap: 'anywhere' }}>
          帖子详情
        </Typography>
        <Tooltip title="刷新">
          <IconButton onClick={() => void loadBase()} aria-label="刷新">
            <RefreshIcon />
          </IconButton>
        </Tooltip>
      </Stack>
      {error && (
        <Alert severity="error" sx={{ mb: 2 }}>
          {error}
        </Alert>
      )}
      {detail && (
        <Stack spacing={2}>
          <PostDisplay postInfo={detail} onImageClick={setLightboxImage} />
          {detail.post.retweeted_status && (
            <Card variant="outlined" sx={{ borderRadius: 1 }}>
              <CardContent>
                <Typography variant="h6">转发层级</Typography>
                <RetweetLevel post={detail.post.retweeted_status} />
              </CardContent>
            </Card>
          )}

          <Card variant="outlined" sx={{ borderRadius: 1 }}>
            <CardContent>
              <Typography variant="h6" mb={1.5}>
                媒体状态
              </Typography>
              {media.length === 0 ? (
                <Typography color="text.secondary">此帖子没有媒体记录。</Typography>
              ) : (
                <Stack divider={<Divider flexItem />} spacing={1.5}>
                  {media.map(item => (
                    <Stack
                      key={item.id}
                      direction={{ xs: 'column', sm: 'row' }}
                      justifyContent="space-between"
                      gap={1}
                    >
                      <Box sx={{ minWidth: 0, flex: 1 }}>
                        <Stack direction="row" spacing={1} alignItems="center">
                          <Chip size="small" label={item.status} color={mediaColor(item.status)} />
                          <Typography variant="body2">
                            {item.media_type}
                            {item.definition ? ` · ${item.definition}` : ''}
                          </Typography>
                        </Stack>
                        <Typography
                          variant="caption"
                          color="text.secondary"
                          sx={{ display: 'block' }}
                        >
                          {item.local_available ? '本地媒体已记录' : '仅远程媒体可用'}
                        </Typography>
                        <UnifiedMediaPreview item={item} />
                      </Box>
                      {item.status === 'failed' && (
                        <Tooltip title="重试媒体">
                          <IconButton color="primary" onClick={() => void handleRetryMedia(item)}>
                            <ReplayIcon />
                          </IconButton>
                        </Tooltip>
                      )}
                    </Stack>
                  ))}
                </Stack>
              )}
            </CardContent>
          </Card>

          <Card variant="outlined" sx={{ borderRadius: 1 }}>
            <CardContent>
              <Typography variant="h6">评论</Typography>
              <Typography variant="body2" color="text.secondary" mb={2}>
                仅加载一级评论；展开后持久采集并查询回复。
              </Typography>
              <FormControl fullWidth size="small" sx={{ mb: 2 }}>
                <InputLabel id="comment-account-label">采集账号</InputLabel>
                <Select
                  labelId="comment-account-label"
                  label="采集账号"
                  value={selectedAccountId}
                  onChange={event => setSelectedAccountId(event.target.value)}
                >
                  <MenuItem value="">
                    <em>请选择账号</em>
                  </MenuItem>
                  {accounts.map(account => (
                    <MenuItem key={account.id} value={account.id}>
                      {account.display_name || account.uid}
                    </MenuItem>
                  ))}
                </Select>
              </FormControl>
              {accounts.length === 0 && (
                <Alert severity="warning" sx={{ mb: 2 }}>
                  没有已启用且配置会话的账号，请先前往同步中心配置。
                </Alert>
              )}
              {roots.length === 0 ? (
                <Typography color="text.secondary">暂无一级评论。</Typography>
              ) : (
                <Stack divider={<Divider flexItem />} spacing={1}>
                  {roots.map(root => {
                    const state = rootStates[root.id]
                    return (
                      <Box key={root.id} py={1}>
                        <Stack direction="row" spacing={1.5} alignItems="flex-start">
                          <Avatar sx={{ width: 34, height: 34 }} />
                          <Box sx={{ flex: 1, minWidth: 0 }}>
                            <Stack direction="row" spacing={1} alignItems="center" flexWrap="wrap">
                              <Typography fontWeight={500}>{root.user_id || '未知用户'}</Typography>
                              <Chip
                                size="small"
                                variant="outlined"
                                label={`${root.child_count} 回复`}
                              />
                            </Stack>
                            <Typography
                              variant="body2"
                              sx={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}
                            >
                              {root.text}
                            </Typography>
                            <Typography variant="caption" color="text.secondary">
                              {new Date(root.created_at).toLocaleString()} · 赞 {root.like_count}
                            </Typography>
                          </Box>
                          <Tooltip title={state?.open ? '收起回复' : '加载回复'}>
                            <IconButton
                              onClick={() => toggleRoot(root)}
                              aria-label="加载回复"
                              sx={{ transform: state?.open ? 'rotate(180deg)' : 'none' }}
                            >
                              <ExpandMoreIcon />
                            </IconButton>
                          </Tooltip>
                        </Stack>
                        {state?.open && (
                          <Box
                            sx={{
                              ml: { xs: 2, sm: 6 },
                              mt: 1.5,
                              pl: 2,
                              borderLeft: '2px solid',
                              borderColor: 'divider',
                            }}
                          >
                            {state.loading && (
                              <Stack spacing={1}>
                                <LinearProgress />
                                <Typography variant="caption">
                                  正在等待持久采集任务完成...
                                </Typography>
                              </Stack>
                            )}
                            {state.error && (
                              <Alert
                                severity="error"
                                action={
                                  <Button
                                    size="small"
                                    startIcon={<ReplayIcon />}
                                    onClick={() =>
                                      void collectAndLoad(root, state.page).catch(() => undefined)
                                    }
                                  >
                                    重试
                                  </Button>
                                }
                              >
                                {state.error}
                              </Alert>
                            )}
                            {!state.loading &&
                              !state.error &&
                              state.items.map(child => (
                                <Box key={child.id} mb={1.5}>
                                  <Typography variant="body2" fontWeight={500}>
                                    {child.user_id || '未知用户'}
                                  </Typography>
                                  <Typography
                                    variant="body2"
                                    sx={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}
                                  >
                                    {child.text}
                                  </Typography>
                                  <Typography variant="caption" color="text.secondary">
                                    {new Date(child.created_at).toLocaleString()}
                                  </Typography>
                                </Box>
                              ))}
                            {state.total > COMMENTS_PER_PAGE && (
                              <Pagination
                                size="small"
                                count={Math.ceil(state.total / COMMENTS_PER_PAGE)}
                                page={state.page}
                                onChange={(_, page) => {
                                  void queryChildren(root.id, page).catch(() => undefined)
                                }}
                              />
                            )}
                          </Box>
                        )}
                      </Box>
                    )
                  })}
                </Stack>
              )}
              {rootTotal > COMMENTS_PER_PAGE && (
                <Pagination
                  sx={{ mt: 2 }}
                  count={Math.ceil(rootTotal / COMMENTS_PER_PAGE)}
                  page={rootPage}
                  onChange={(_, page) => setRootPage(page)}
                />
              )}
            </CardContent>
          </Card>
        </Stack>
      )}
      {lightboxImage && (
        <Box
          sx={{
            position: 'fixed',
            inset: 0,
            zIndex: theme => theme.zIndex.modal,
            bgcolor: 'rgba(0,0,0,.82)',
            display: 'grid',
            placeItems: 'center',
          }}
        >
          <FullSizeImage image={lightboxImage} onClose={() => setLightboxImage(null)} />
        </Box>
      )}
    </Box>
  )
}

export default PostDetail
