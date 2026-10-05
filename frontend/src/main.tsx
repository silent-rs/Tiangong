import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import { setupMarkdownLinkify } from './utils/markdownLinkify'
import App from './App'
import { isRemoteHost } from './api/remote'
import { installRemoteViewport } from './lib/remoteViewport'

// 手机 H5（远程模式）：先固定视口，避免输入时页面缩放、发送按钮被顶出屏幕
if (isRemoteHost()) installRemoteViewport()

// 必须先于任何 Markdown 预览挂载执行（修正链接识别，全局一次）
setupMarkdownLinkify()

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
