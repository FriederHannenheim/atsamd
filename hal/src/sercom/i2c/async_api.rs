use crate::{
    async_hal::interrupts::{Binding, Handler, InterruptSource},
    sercom::{
        i2c::{self, AnyConfig, Flags, I2c},
        Sercom,
    },
    typelevel::NoneT,
};
use core::{marker::PhantomData, task::Poll};
use cortex_m::interrupt::InterruptNumber;

/// Interrupt handler for async I2C operarions
pub struct InterruptHandler<S: Sercom> {
    _private: (),
    _sercom: PhantomData<S>,
}

impl<S: Sercom> Handler<S::Interrupt> for InterruptHandler<S> {
    // TODO the ISR gets called twice on every MB request???
    #[inline]
    unsafe fn on_interrupt() {
        let mut peripherals = unsafe { crate::pac::Peripherals::steal() };
        let i2cm = S::reg_block(&mut peripherals).i2cm();
        let flags_to_check = Flags::all();
        let flags_pending = Flags::from_bits_truncate(i2cm.intflag.read().bits());

        // Disable interrupts, but don't clear the flags. The future will take care of
        // clearing flags and re-enabling interrupts when woken.
        if flags_to_check.contains(flags_pending) {
            i2cm.intenclr
                .write(|w| unsafe { w.bits(flags_pending.bits()) });
            S::rx_waker().wake();
        }
    }
}

impl<C, S> I2c<C>
where
    C: AnyConfig<Sercom = S>,
    S: Sercom,
{
    /// Turn an [`I2c`] into an [`I2cFuture`]
    #[inline]
    pub fn into_future<I>(self, _interrupts: I) -> I2cFuture<C>
    where
        I: Binding<S::Interrupt, InterruptHandler<S>>,
    {
        S::Interrupt::unpend();
        unsafe { S::Interrupt::enable() };

        I2cFuture {
            i2c: self,
            _dma_channel: NoneT,
        }
    }
}

/// `async` version of [`I2c`].
///
/// Create this struct by calling [`I2c::into_future`](I2c::into_future).
pub struct I2cFuture<C, D = NoneT>
where
    C: AnyConfig,
{
    pub(in super::super) i2c: I2c<C>,
    _dma_channel: D,
}

#[cfg(feature = "dma")]
/// Convenience type for a [`I2cFuture`] in DMA
/// mode. The type parameter `I` represents the DMA channel ID (`ChX`).
pub type I2cFutureDma<C, I> = I2cFuture<C, crate::dmac::Channel<I, crate::dmac::ReadyFuture>>;

impl<C, S, D> I2cFuture<C, D>
where
    C: AnyConfig<Sercom = S>,
    S: Sercom,
{
    async fn wait_flags(&mut self, flags_to_wait: Flags) {
        core::future::poll_fn(|cx| {
            // Scope maybe_pending so we don't forget to re-poll the register later down.
            {
                let maybe_pending = self.i2c.config.as_ref().registers.read_flags();
                if flags_to_wait.intersects(maybe_pending) {
                    return Poll::Ready(());
                }
            }

            self.i2c.disable_interrupts(Flags::all());
            // By convention, I2C uses the sercom's RX waker.
            S::rx_waker().register(cx.waker());
            self.i2c.enable_interrupts(flags_to_wait);
            let maybe_pending = self.i2c.config.as_ref().registers.read_flags();

            if !flags_to_wait.intersects(maybe_pending) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }
}

impl<C, S> I2cFuture<C, NoneT>
where
    C: AnyConfig<Sercom = S>,
    S: Sercom,
{
    /// Use a DMA channel for reads/writes
    #[cfg(feature = "dma")]
    pub fn with_dma_channel<D: crate::dmac::AnyChannel<Status = crate::dmac::ReadyFuture>>(
        self,
        dma_channel: D,
    ) -> I2cFuture<C, D> {
        I2cFuture {
            i2c: self.i2c,
            _dma_channel: dma_channel,
        }
    }

    /// Return the underlying [`I2c`].
    pub fn free(self) -> I2c<C> {
        self.i2c
    }

    /// Asynchronously write from a buffer.
    #[inline]
    pub async fn write(&mut self, addr: u8, bytes: &[u8]) -> Result<(), i2c::Error> {
        self.i2c.config.as_mut().registers.start_write(addr)?;

        for b in bytes {
            self.wait_flags(Flags::MB | Flags::ERROR).await;
            self.i2c.read_status().check_bus_error()?;

            self.i2c.config.as_mut().registers.write_one(*b);
        }

        self.i2c.cmd_stop();

        Ok(())
    }

    /// Asynchronously read into a buffer.
    #[inline]
    pub async fn read(&mut self, addr: u8, buffer: &mut [u8]) -> Result<(), i2c::Error> {
        self.i2c.config.as_mut().registers.start_read(addr)?;

        // Some manual iterator gumph because we need to ack bytes after the first.
        let mut iter = buffer.iter_mut();
        *iter.next().expect("buffer len is at least 1") = self.read_one().await;

        loop {
            match iter.next() {
                None => break,
                Some(dest) => {
                    // Ack the last byte so we can receive another one
                    self.i2c.config.as_mut().registers.cmd_read();
                    *dest = self.read_one().await;
                }
            }
        }

        // Arrange to send NACK on next command to
        // stop slave from transmitting more data
        self.i2c
            .config
            .as_mut()
            .registers
            .i2c_master()
            .ctrlb
            .modify(|_, w| w.ackact().set_bit());

        Ok(())
    }

    /// Asynchronously write from a buffer, then read into a buffer. This is an
    /// extremely common pattern: writing a register address, then
    /// read its value from the slave.
    #[inline]
    pub async fn write_read(
        &mut self,
        addr: u8,
        write_buf: &[u8],
        read_buf: &mut [u8],
    ) -> Result<(), i2c::Error> {
        self.write(addr, write_buf).await?;
        self.read(addr, read_buf).await?;
        Ok(())
    }

    async fn read_one(&mut self) -> u8 {
        self.wait_flags(Flags::SB | Flags::ERROR).await;
        self.i2c.config.as_mut().registers.read_one()
    }
}

// impl<C, N, D> Drop for I2cFuture<C, N, D>
// where
//     C: AnyConfig,
//     N: InterruptNumber,
// {
//     #[inline]
//     fn drop(&mut self) {
//         cortex_m::peripheral::NVIC::mask(self.irq_number);
//     }
// }

impl<C, N> AsRef<I2c<C>> for I2cFuture<C, N>
where
    C: AnyConfig,
    N: InterruptNumber,
{
    #[inline]
    fn as_ref(&self) -> &I2c<C> {
        &self.i2c
    }
}

impl<C, N> AsMut<I2c<C>> for I2cFuture<C, N>
where
    C: AnyConfig,
    N: InterruptNumber,
{
    #[inline]
    fn as_mut(&mut self) -> &mut I2c<C> {
        &mut self.i2c
    }
}

mod impl_ehal {
    use super::*;
    use embedded_hal_async::i2c::{ErrorType, I2c as I2cTrait, Operation};

    impl<C, D> ErrorType for I2cFuture<C, D>
    where
        C: AnyConfig,
    {
        type Error = i2c::Error;
    }

    impl<C> I2cTrait for I2cFuture<C>
    where
        C: AnyConfig,
    {
        #[inline]
        async fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), Self::Error> {
            self.read(address, buffer).await?;
            Ok(())
        }

        #[inline]
        async fn write(&mut self, address: u8, bytes: &[u8]) -> Result<(), Self::Error> {
            self.write(address, bytes).await?;
            Ok(())
        }

        #[inline]
        async fn write_read(
            &mut self,
            address: u8,
            wr_buffer: &[u8],
            rd_buffer: &mut [u8],
        ) -> Result<(), Self::Error> {
            self.write_read(address, wr_buffer, rd_buffer).await?;
            Ok(())
        }

        #[inline]
        async fn transaction<'mut_self, 'a, 'b>(
            &'mut_self mut self,
            address: u8,
            operations: &'a mut [embedded_hal_async::i2c::Operation<'b>],
        ) -> Result<(), Self::Error> {
            for op in operations {
                match op {
                    Operation::Read(buf) => self.read(address, buf).await?,
                    Operation::Write(buf) => self.write(address, buf).await?,
                }
            }

            Ok(())
        }
    }

    #[cfg(feature = "dma")]
    impl<C, D> I2cTrait for I2cFuture<C, D>
    where
        C: AnyConfig,
        D: crate::dmac::AnyChannel<Status = crate::dmac::ReadyFuture>,
    {
        #[inline]
        async fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), Self::Error> {
            self.read(address, buffer).await?;
            Ok(())
        }

        #[inline]
        async fn write(&mut self, address: u8, bytes: &[u8]) -> Result<(), Self::Error> {
            self.write(address, bytes).await?;
            Ok(())
        }

        #[inline]
        async fn write_read(
            &mut self,
            address: u8,
            wr_buffer: &[u8],
            rd_buffer: &mut [u8],
        ) -> Result<(), Self::Error> {
            self.write_read(address, wr_buffer, rd_buffer).await?;
            Ok(())
        }

        #[inline]
        async fn transaction<'mut_self, 'a, 'b>(
            &'mut_self mut self,
            address: u8,
            operations: &'a mut [embedded_hal_async::i2c::Operation<'b>],
        ) -> Result<(), Self::Error> {
            for op in operations {
                match op {
                    Operation::Read(buf) => self.read(address, buf).await?,
                    Operation::Write(buf) => self.write(address, buf).await?,
                }
            }

            Ok(())
        }
    }
}

#[cfg(feature = "dma")]
mod dma {
    use super::*;
    use crate::dmac::{AnyChannel, ReadyFuture};
    use crate::sercom::async_dma::{read_dma, write_dma, SercomPtr};

    impl<C, S, D> I2cFuture<C, D>
    where
        C: AnyConfig<Sercom = S>,
        S: Sercom,
        D: AnyChannel<Status = ReadyFuture>,
    {
        fn sercom_ptr(&self) -> SercomPtr<i2c::Word> {
            SercomPtr(self.i2c.data_ptr())
        }

        /// Asynchronously write from a buffer using DMA.
        #[inline]
        pub async fn write(&mut self, address: u8, bytes: &[u8]) -> Result<(), i2c::Error> {
            self.i2c.init_dma_transfer()?;

            // SAFETY: Using SercomPtr and ImmutableSlice is safe because we hold on
            // to &mut self and bytes as long as the transfer hasn't completed.
            let i2c_ptr = self.sercom_ptr();

            let len = bytes.len();
            assert!(len > 0 && len <= 255);
            self.i2c.start_dma_write(address, len as u8);

            write_dma::<_, S>(&mut self._dma_channel, i2c_ptr, bytes)
                .await
                .map_err(i2c::Error::Dma)?;

            Ok(())
        }

        /// Asynchronously read into a buffer using DMA.
        #[inline]
        pub async fn read(&mut self, address: u8, buffer: &mut [u8]) -> Result<(), i2c::Error> {
            self.i2c.init_dma_transfer()?;

            // SAFETY: Using SercomPtr is safe because we hold on
            // to &mut self as long as the transfer hasn't completed.
            let i2c_ptr = self.sercom_ptr();

            let len = buffer.len();
            assert!(len > 0 && len <= 255);
            self.i2c.start_dma_read(address, len as u8);

            read_dma::<_, S>(&mut self._dma_channel, i2c_ptr, buffer)
                .await
                .map_err(i2c::Error::Dma)?;

            Ok(())
        }

        /// Asynchronously write from a buffer, then read into a buffer, all
        /// using DMA. This is an extremely common pattern: writing a
        /// register address, then read its value from the slave.
        #[inline]
        pub async fn write_read(
            &mut self,
            addr: u8,
            write_buf: &[u8],
            read_buf: &mut [u8],
        ) -> Result<(), i2c::Error> {
            self.write(addr, write_buf).await?;
            // TODO may need some sort of delay here??
            self.read(addr, read_buf).await?;
            Ok(())
        }
    }
}