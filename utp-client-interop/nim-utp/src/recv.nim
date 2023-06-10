# Copyright (c) 2020-2023 Status Research & Development GmbH
# Licensed and distributed under either of
#   * MIT license (license terms in the root directory or at https://opensource.org/licenses/MIT).
#   * Apache v2 license (license terms in the root directory or at https://www.apache.org/licenses/LICENSE-2.0).
# at your option. This file may not be copied, modified, or distributed except according to those terms.

{.push raises: [].}

import
  chronos, stew/[results, byteutils],
  eth/utp/utp_router,
  eth/utp/utp_socket,
  eth/utp/utp_protocol

# Example application to interact with the reference implementation server
# to be able to test against the reference implementation.
# To run libutp server:
# 1. git clone https://github.com/bittorrent/libutp.git
# 2. cd libutp
# 3. make
# 4. ./ucat -ddddd -l -p 9078 - it will run utp server on port 9078
when isMainModule:
  proc echoIncomingSocketCallBack(): AcceptConnectionCallback[TransportAddress] =
    return (
      proc (server: UtpRouter[TransportAddress],
          client: UtpSocket[TransportAddress]):
          Future[void] {.gcsafe, raises: [].} =
        echo "received incoming connection"
        let fakeFuture = newFuture[void]()
        fakeFuture.complete()
        return fakeFuture
    )
  # TODO read client/server ports and address from cmd line or config file
  let localAddress = initTAddress("0.0.0.0", 9078)
  let utpProt = UtpProtocol.new(echoIncomingSocketCallBack(), localAddress)

  waitFor(sleepAsync(milliseconds(1000)))

  let bytes = await utpProt.read(10)
  echo "Hi, ", bytes, "!"

  # discard waitFor soc.write(bytes)

  # waitFor(sleepAsync(milliseconds(1000)))

  # discard waitFor soc.write(bytes)

  runForever()

  waitFor utpProt.shutdownWait()