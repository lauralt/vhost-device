use super::{
    rxops::*,
    rxqueue::*,
    txbuf::*,
    vhu_vsock::{
        Error, Result, CONN_TX_BUF_SIZE, VSOCK_FLAGS_SHUTDOWN_RCV, VSOCK_FLAGS_SHUTDOWN_SEND,
        VSOCK_OP_CREDIT_REQUEST, VSOCK_OP_CREDIT_UPDATE, VSOCK_OP_REQUEST, VSOCK_OP_RESPONSE,
        VSOCK_OP_RST, VSOCK_OP_RW, VSOCK_OP_SHUTDOWN, VSOCK_TYPE_STREAM,
    },
    vhu_vsock_thread::VhostUserVsockThread,
};
use log::info;
use std::{
    io::{ErrorKind, Read, Write},
    num::Wrapping,
    os::unix::prelude::{AsRawFd, RawFd},
};
use virtio_vsock::packet::VsockPacket;
use vm_memory::{Bytes, bitmap::BitmapSlice};

#[derive(Debug)]
pub struct VsockConnection<S> {
    /// Host-side stream corresponding to this vsock connection.
    pub stream: S,
    /// Specifies if the stream is connected to a listener on the host.
    pub connect: bool,
    /// Port at which a guest application is listening to.
    pub peer_port: u32,
    /// Queue holding pending rx operations per connection.
    pub rx_queue: RxQueue,
    /// CID of the host.
    local_cid: u64,
    /// Port on the host at which a host-side application listens to.
    pub local_port: u32,
    /// CID of the guest.
    pub guest_cid: u64,
    /// Total number of bytes written to stream from tx buffer.
    pub fwd_cnt: Wrapping<u32>,
    /// Total number of bytes previously forwarded to stream.
    last_fwd_cnt: Wrapping<u32>,
    /// Size of buffer the guest has allocated for this connection.
    peer_buf_alloc: u32,
    /// Number of bytes the peer has forwarded to a connection.
    peer_fwd_cnt: Wrapping<u32>,
    /// The total number of bytes sent to the guest vsock driver.
    rx_cnt: Wrapping<u32>,
    /// epoll fd to which this connection's stream has to be added.
    pub epoll_fd: RawFd,
    /// Local tx buffer.
    pub tx_buf: LocalTxBuf,
}

impl<S: AsRawFd + Read + Write> VsockConnection<S> {
    /// Create a new vsock connection object for locally i.e host-side
    /// inititated connections.
    pub fn new_local_init(
        stream: S,
        local_cid: u64,
        local_port: u32,
        guest_cid: u64,
        guest_port: u32,
        epoll_fd: RawFd,
    ) -> Self {
        Self {
            stream,
            connect: false,
            peer_port: guest_port,
            rx_queue: RxQueue::new(),
            local_cid,
            local_port,
            guest_cid,
            fwd_cnt: Wrapping(0),
            last_fwd_cnt: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            epoll_fd,
            tx_buf: LocalTxBuf::new(),
        }
    }

    /// Create a new vsock connection object for connections initiated by
    /// an application running in the guest.
    pub fn new_peer_init(
        stream: S,
        local_cid: u64,
        local_port: u32,
        guest_cid: u64,
        guest_port: u32,
        epoll_fd: RawFd,
        peer_buf_alloc: u32,
    ) -> Self {
        let mut rx_queue = RxQueue::new();
        rx_queue.enqueue(RxOps::Response);
        Self {
            stream,
            connect: false,
            peer_port: guest_port,
            rx_queue,
            local_cid,
            local_port,
            guest_cid,
            fwd_cnt: Wrapping(0),
            last_fwd_cnt: Wrapping(0),
            peer_buf_alloc,
            peer_fwd_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
            epoll_fd,
            tx_buf: LocalTxBuf::new(),
        }
    }

    /// Set the peer port to the guest side application's port.
    pub fn set_peer_port(&mut self, peer_port: u32) {
        self.peer_port = peer_port;
    }

    /// Process a vsock packet that is meant for this connection.
    /// Forward data to the host-side application if the vsock packet
    /// contains a RW operation.
    pub(crate) fn recv_pkt<'a, B: BitmapSlice>
        (&mut self, pkt: &'a mut VsockPacket<'a, B>)
         -> Result<()> {
        // Initialize all fields in the packet header
        self.init_pkt(pkt);

        match self.rx_queue.dequeue() {
            Some(RxOps::Request) => {
                // Send a connection request to the guest-side application
                pkt.set_op(VSOCK_OP_REQUEST);
                Ok(())
            }
            Some(RxOps::Rw) => {
                if !self.connect {
                    // There is no host-side application listening for this
                    // packet, hence send back an RST.
                    pkt.set_op(VSOCK_OP_RST);
                    return Ok(());
                }

                // Check if peer has space for receiving data
                if self.need_credit_update_from_peer() {
                    self.last_fwd_cnt = self.fwd_cnt;
                    pkt.set_op(VSOCK_OP_CREDIT_REQUEST);
                    return Ok(());
                }

                let buf = pkt.data().ok_or(Error::PktBufMissing)?;

                // Perform a credit check to find the maximum read size. The read
                // data must fit inside a packet buffer and be within peer's
                // available buffer space
                let max_read_len = std::cmp::min(buf.len(), self.peer_avail_credit());

                // Read data from the stream directly into the buffer
                if let Ok(read_cnt) = buf.read_from(0, &mut self.stream, max_read_len) {
                    if read_cnt == 0 {
                        // If no data was read then the stream was closed down unexpectedly.
                        // Send a shutdown packet to the guest-side application.
                        pkt.set_op(VSOCK_OP_SHUTDOWN)
                            .set_flag(VSOCK_FLAGS_SHUTDOWN_RCV)
                            .set_flag(VSOCK_FLAGS_SHUTDOWN_SEND);
                    } else {
                        // If data was read, then set the length field in the packet header
                        // to the amount of data that was read.
                        pkt.set_op(VSOCK_OP_RW).set_len(read_cnt as u32);

                        // Re-register the stream file descriptor for read and write events
                        VhostUserVsockThread::epoll_register(
                            self.epoll_fd,
                            self.stream.as_raw_fd(),
                            epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                        )?;
                    }

                    // Update the rx_cnt with the amount of data in the vsock packet.
                    self.rx_cnt += Wrapping(pkt.len());
                    self.last_fwd_cnt = self.fwd_cnt;
                }
                Ok(())
            }
            Some(RxOps::Response) => {
                // A response has been received to a newly initiated host-side connection
                self.connect = true;
                pkt.set_op(VSOCK_OP_RESPONSE);
                Ok(())
            }
            Some(RxOps::CreditUpdate) => {
                // Request credit update from the guest.
                if !self.rx_queue.pending_rx() {
                    // Waste an rx buffer if no rx is pending
                    pkt.set_op(VSOCK_OP_CREDIT_UPDATE);
                    self.last_fwd_cnt = self.fwd_cnt;
                }
                Ok(())
            }
            _ => Err(Error::NoRequestRx),
        }
    }

    /// Deliver a guest generated packet to this connection.
    ///
    /// Returns:
    /// - always `Ok(())` to indicate that the packet has been consumed
    pub(crate) fn send_pkt<'a, B: BitmapSlice>
        (&mut self, pkt:  &VsockPacket<'a, B>) -> Result<()>
    {
        // Update peer credit information
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        match pkt.op() {
            VSOCK_OP_RESPONSE => {
                // Confirmation for a host initiated connection
                // TODO: Handle stream write error in a better manner
                let response = format!("OK {}\n", self.peer_port);
                self.stream.write_all(response.as_bytes()).unwrap();
                self.connect = true;
            }
            VSOCK_OP_RW => {
                // Data has to be written to the host-side stream
                if pkt.data().is_none() {
                    info!(
                        "Dropping empty packet from guest (lp={}, pp={})",
                        self.local_port, self.peer_port
                    );
                    return Ok(());
                }

                let buf_slice = &pkt.buf().unwrap()[..(pkt.len() as usize)];
                if let Err(err) = self.send_bytes(buf_slice) {
                    // TODO: Terminate this connection
                    dbg!("err:{:?}", err);
                    return Ok(());
                }
            }
            VSOCK_OP_CREDIT_UPDATE => {
                // Already updated the credit

                // Re-register the stream file descriptor for read and write events
                if VhostUserVsockThread::epoll_modify(
                    self.epoll_fd,
                    self.stream.as_raw_fd(),
                    epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                )
                .is_err()
                {
                    VhostUserVsockThread::epoll_register(
                        self.epoll_fd,
                        self.stream.as_raw_fd(),
                        epoll::Events::EPOLLIN | epoll::Events::EPOLLOUT,
                    )
                    .unwrap();
                };
            }
            VSOCK_OP_CREDIT_REQUEST => {
                // Send back this connection's credit information
                self.rx_queue.enqueue(RxOps::CreditUpdate);
            }
            VSOCK_OP_SHUTDOWN => {
                // Shutdown this connection
                let recv_off = pkt.flags() & VSOCK_FLAGS_SHUTDOWN_RCV != 0;
                let send_off = pkt.flags() & VSOCK_FLAGS_SHUTDOWN_SEND != 0;

                if recv_off && send_off && self.tx_buf.is_empty() {
                    self.rx_queue.enqueue(RxOps::Reset);
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Write data to the host-side stream.
    ///
    /// Returns:
    /// - Ok(cnt) where cnt is the number of bytes written to the stream
    /// - Err(Error::UnixWrite) if there was an error writing to the stream
    fn send_bytes(&mut self, buf: &[u8]) -> Result<()> {
        if !self.tx_buf.is_empty() {
            // Data is already present in the buffer and the backend
            // is waiting for a EPOLLOUT event to flush it
            return self.tx_buf.push(buf);
        }

        // Write data to the stream
        let written_count = match self.stream.write(buf) {
            Ok(cnt) => cnt,
            Err(e) => {
                if e.kind() == ErrorKind::WouldBlock {
                    0
                } else {
                    println!("send_bytes error: {:?}", e);
                    return Err(Error::UnixWrite);
                }
            }
        };

        // Increment forwarded count by number of bytes written to the stream
        self.fwd_cnt += Wrapping(written_count as u32);
        // TODO: https://github.com/torvalds/linux/commit/c69e6eafff5f725bc29dcb8b52b6782dca8ea8a2
        self.rx_queue.enqueue(RxOps::CreditUpdate);

        if written_count != buf.len() {
            return self.tx_buf.push(&buf[written_count..]);
        }

        Ok(())
    }

    /// Initialize all header fields in the vsock packet.
    fn init_pkt<'a, B:BitmapSlice>
        (&self, pkt: &'a mut VsockPacket<'a, B>) ->
        &'a mut VsockPacket<'a, B>
    {
        // Zero out the packet header
        // for b in pkt.hdr_mut() {
        //     *b = 0;
        // }

        pkt.set_src_cid(self.local_cid)
            .set_dst_cid(self.guest_cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(VSOCK_TYPE_STREAM)
            .set_buf_alloc(CONN_TX_BUF_SIZE)
            .set_fwd_cnt(self.fwd_cnt.0)
    }

    /// Get max number of bytes we can send to peer without overflowing
    /// the peer's buffer.
    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc as u32) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    /// Check if we need a credit update from the peer before sending
    /// more data to it.
    fn need_credit_update_from_peer(&self) -> bool {
        self.peer_avail_credit() == 0
    }
}

#[cfg(test)]
mod tests {
    use byteorder::{ByteOrder, LittleEndian};

    use super::*;
    use crate::packet::tests::{prepare_desc_chain_vsock, HeadParams};
    use crate::vhu_vsock::VSOCK_HOST_CID;

    struct VsockDummySocket {
        data: Vec<u8>,
    }

    impl VsockDummySocket {
        fn new() -> Self {
            Self { data: Vec::new() }
        }
    }

    impl Write for VsockDummySocket {
        fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
            self.data.clear();
            self.data.extend_from_slice(buf);

            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Read for VsockDummySocket {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf[..self.data.len()].copy_from_slice(&self.data);
            Ok(self.data.len())
        }
    }

    impl AsRawFd for VsockDummySocket {
        fn as_raw_fd(&self) -> RawFd {
            -1
        }
    }

    #[test]
    fn test_vsock_conn_init() {
        // new locally inititated connection
        let dummy_file = VsockDummySocket::new();
        let mut vsock_conn_local =
            VsockConnection::new_local_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1);

        assert!(!vsock_conn_local.connect);
        assert_eq!(vsock_conn_local.peer_port, 5001);
        assert_eq!(vsock_conn_local.rx_queue, RxQueue::new());
        assert_eq!(vsock_conn_local.local_cid, VSOCK_HOST_CID);
        assert_eq!(vsock_conn_local.local_port, 5000);
        assert_eq!(vsock_conn_local.guest_cid, 3);

        // set peer port
        vsock_conn_local.set_peer_port(5002);
        assert_eq!(vsock_conn_local.peer_port, 5002);

        // New connection initiated by the peer/guest
        let dummy_file = VsockDummySocket::new();
        let mut vsock_conn_peer =
            VsockConnection::new_peer_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1, 65536);

        assert!(!vsock_conn_peer.connect);
        assert_eq!(vsock_conn_peer.peer_port, 5001);
        assert_eq!(vsock_conn_peer.rx_queue.dequeue().unwrap(), RxOps::Response);
        assert!(!vsock_conn_peer.rx_queue.pending_rx());
        assert_eq!(vsock_conn_peer.local_cid, VSOCK_HOST_CID);
        assert_eq!(vsock_conn_peer.local_port, 5000);
        assert_eq!(vsock_conn_peer.guest_cid, 3);
        assert_eq!(vsock_conn_peer.peer_buf_alloc, 65536);
    }

    #[test]
    fn test_vsock_conn_credit() {
        // new locally inititated connection
        let dummy_file = VsockDummySocket::new();
        let mut vsock_conn_local =
            VsockConnection::new_local_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1);

        assert_eq!(vsock_conn_local.peer_avail_credit(), 0);
        assert!(vsock_conn_local.need_credit_update_from_peer());

        vsock_conn_local.peer_buf_alloc = 65536;
        assert_eq!(vsock_conn_local.peer_avail_credit(), 65536);
        assert!(!vsock_conn_local.need_credit_update_from_peer());

        vsock_conn_local.rx_cnt = Wrapping(32768);
        assert_eq!(vsock_conn_local.peer_avail_credit(), 32768);
        assert!(!vsock_conn_local.need_credit_update_from_peer());

        vsock_conn_local.rx_cnt = Wrapping(65536);
        assert_eq!(vsock_conn_local.peer_avail_credit(), 0);
        assert!(vsock_conn_local.need_credit_update_from_peer());
    }

    #[test]
    fn test_vsock_conn_init_pkt() {
        // parameters for packet head construction
        let head_params = HeadParams::new(VSOCK_PKT_HDR_SIZE, 10);

        // new locally inititated connection
        let dummy_file = VsockDummySocket::new();
        let vsock_conn_local =
            VsockConnection::new_local_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1);

        // write only descriptor chain
        let (mem, mut descr_chain) = prepare_desc_chain_vsock(true, &head_params, 2, 10);
        let mut vsock_pkt = VsockPacket::from_rx_virtq_head(&mut descr_chain, mem).unwrap();

        // initialize a vsock packet for the guest
        vsock_conn_local.init_pkt(&mut vsock_pkt);

        assert_eq!(vsock_pkt.src_cid(), VSOCK_HOST_CID);
        assert_eq!(vsock_pkt.dst_cid(), 3);
        assert_eq!(vsock_pkt.src_port(), 5000);
        assert_eq!(vsock_pkt.dst_port(), 5001);
        assert_eq!(vsock_pkt.pkt_type(), VSOCK_TYPE_STREAM);
        assert_eq!(vsock_pkt.buf_alloc(), CONN_TX_BUF_SIZE);
        assert_eq!(vsock_pkt.fwd_cnt(), 0);
    }

    #[test]
    fn test_vsock_conn_recv_pkt() {
        // parameters for packet head construction
        let head_params = HeadParams::new(VSOCK_PKT_HDR_SIZE, 5);

        // new locally inititated connection
        let dummy_file = VsockDummySocket::new();
        let mut vsock_conn_local =
            VsockConnection::new_local_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1);

        // write only descriptor chain
        let (mem, mut descr_chain) = prepare_desc_chain_vsock(true, &head_params, 1, 5);
        let mut vsock_pkt = VsockPacket::from_rx_virtq_head(&mut descr_chain, mem).unwrap();

        // VSOCK_OP_REQUEST: new local conn request
        vsock_conn_local.rx_queue.enqueue(RxOps::Request);
        let vsock_op_req = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_req.is_ok());
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_REQUEST);

        // VSOCK_OP_RST: reset if connection not established
        vsock_conn_local.rx_queue.enqueue(RxOps::Rw);
        let vsock_op_rst = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_rst.is_ok());
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_RST);

        // VSOCK_OP_CREDIT_UPDATE: need credit update from peer/guest
        vsock_conn_local.connect = true;
        vsock_conn_local.rx_queue.enqueue(RxOps::Rw);
        vsock_conn_local.fwd_cnt = Wrapping(1024);
        let vsock_op_credit_update = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_credit_update.is_ok());
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_CREDIT_REQUEST);
        assert_eq!(vsock_conn_local.last_fwd_cnt, Wrapping(1024));

        // VSOCK_OP_SHUTDOWN: zero data read from stream/file
        vsock_conn_local.peer_buf_alloc = 65536;
        vsock_conn_local.rx_queue.enqueue(RxOps::Rw);
        let vsock_op_zero_read_shutdown = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_zero_read_shutdown.is_ok());
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_conn_local.rx_cnt, Wrapping(0));
        assert_eq!(vsock_conn_local.last_fwd_cnt, Wrapping(1024));
        assert_eq!(vsock_pkt.op(), VSOCK_OP_SHUTDOWN);
        assert_eq!(
            vsock_pkt.flags(),
            VSOCK_FLAGS_SHUTDOWN_RCV | VSOCK_FLAGS_SHUTDOWN_SEND
        );

        // VSOCK_OP_RW: finite data read from stream/file
        vsock_conn_local.stream.write_all(b"hello").unwrap();
        vsock_conn_local.rx_queue.enqueue(RxOps::Rw);
        let vsock_op_zero_read = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        // below error due to epoll add
        assert!(vsock_op_zero_read.is_err());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_RW);
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_pkt.len(), 5);
        assert_eq!(vsock_pkt.buf().unwrap(), b"hello");

        // VSOCK_OP_RESPONSE: response from a locally initiated connection
        vsock_conn_local.rx_queue.enqueue(RxOps::Response);
        let vsock_op_response = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_response.is_ok());
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_RESPONSE);
        assert!(vsock_conn_local.connect);

        // VSOCK_OP_CREDIT_UPDATE: guest needs credit update
        vsock_conn_local.rx_queue.enqueue(RxOps::CreditUpdate);
        let vsock_op_credit_update = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(!vsock_conn_local.rx_queue.pending_rx());
        assert!(vsock_op_credit_update.is_ok());
        assert_eq!(vsock_pkt.op(), VSOCK_OP_CREDIT_UPDATE);
        assert_eq!(vsock_conn_local.last_fwd_cnt, Wrapping(1024));

        // non-existent request
        let vsock_op_error = vsock_conn_local.recv_pkt(&mut vsock_pkt);
        assert!(vsock_op_error.is_err());
    }

    #[test]
    fn test_vsock_conn_send_pkt() {
        // parameters for packet head construction
        let head_params = HeadParams::new(VSOCK_PKT_HDR_SIZE, 5);

        // new locally inititated connection
        let dummy_file = VsockDummySocket::new();
        let mut vsock_conn_local =
            VsockConnection::new_local_init(dummy_file, VSOCK_HOST_CID, 5000, 3, 5001, -1);

        // write only descriptor chain
        let (mem, mut descr_chain) = prepare_desc_chain_vsock(false, &head_params, 1, 5);
        let mut vsock_pkt = VsockPacket::from_tx_virtq_head(&mut descr_chain, mem).unwrap();

        // peer credit information
        const HDROFF_BUF_ALLOC: usize = 36;
        const HDROFF_FWD_CNT: usize = 40;
        LittleEndian::write_u32(&mut vsock_pkt.hdr_mut()[HDROFF_BUF_ALLOC..], 65536);
        LittleEndian::write_u32(&mut vsock_pkt.hdr_mut()[HDROFF_FWD_CNT..], 1024);

        // check if peer credit information is updated currently
        let credit_check = vsock_conn_local.send_pkt(&vsock_pkt);
        assert!(credit_check.is_ok());
        assert_eq!(vsock_conn_local.peer_buf_alloc, 65536);
        assert_eq!(vsock_conn_local.peer_fwd_cnt, Wrapping(1024));

        // VSOCK_OP_RESPONSE
        vsock_pkt.set_op(VSOCK_OP_RESPONSE);
        let peer_response = vsock_conn_local.send_pkt(&vsock_pkt);
        assert!(peer_response.is_ok());
        assert!(vsock_conn_local.connect);
        let mut resp_buf = vec![0; 8];
        vsock_conn_local.stream.read_exact(&mut resp_buf).unwrap();
        assert_eq!(resp_buf, b"OK 5001\n");

        // VSOCK_OP_RW
        vsock_pkt.set_op(VSOCK_OP_RW);
        vsock_pkt.buf_mut().unwrap().copy_from_slice(b"hello");
        let rw_response = vsock_conn_local.send_pkt(&vsock_pkt);
        assert!(rw_response.is_ok());
        let mut resp_buf = vec![0; 5];
        vsock_conn_local.stream.read_exact(&mut resp_buf).unwrap();
        assert_eq!(resp_buf, b"hello");

        // VSOCK_OP_CREDIT_REQUEST
        vsock_pkt.set_op(VSOCK_OP_CREDIT_REQUEST);
        let credit_response = vsock_conn_local.send_pkt(&vsock_pkt);
        assert!(credit_response.is_ok());
        assert_eq!(
            vsock_conn_local.rx_queue.peek().unwrap(),
            RxOps::CreditUpdate
        );

        // VSOCK_OP_SHUTDOWN
        vsock_pkt.set_op(VSOCK_OP_SHUTDOWN);
        vsock_pkt.set_flags(VSOCK_FLAGS_SHUTDOWN_RCV | VSOCK_FLAGS_SHUTDOWN_SEND);
        let shutdown_response = vsock_conn_local.send_pkt(&vsock_pkt);
        assert!(shutdown_response.is_ok());
        assert!(vsock_conn_local.rx_queue.contains(RxOps::Reset.bitmask()));
    }
}
