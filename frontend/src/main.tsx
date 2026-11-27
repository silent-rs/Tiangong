import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import { setupMarkdownPreview } from './utils/markdownLinkify'
import App from './App'
import { isRemoteHost } from './api/remote'
import { installRemoteViewport } from './lib/remoteViewport'

// 手机 H5（远程模式）：先固定视口，避免输入时页面缩放、发送按钮被顶出屏幕
if (isRemoteHost()) installRemoteViewport()

// 必须先于任何 Markdown 预览挂载执行（链接修正 + 裸 HTML 防护，全局一次）
setupMarkdownPreview()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
