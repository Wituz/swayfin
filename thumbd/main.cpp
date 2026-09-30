// swayfin-thumbd: makes thumbnails with KDE's KIO thumbnailers and decodes image previews
// with Qt's image readers, so swayfin itself never loads Qt. KIO also stores the
// thumbnails it makes in the shared ~/.cache/thumbnails.
//
// stdin, NUL-terminated records:
//   <path>                         thumbnail for this file
//   0x01                           drop all queued and in-flight thumbnails
//   0x02 <id> " F " <maxw> ' ' <maxh> ' ' <path>
//                                  preview: the whole image decoded to fit maxw x maxh
//                                  (never enlarged)
//   0x02 <id> " R " <x> ' ' <y> ' ' <rw> ' ' <rh> ' ' <w> ' ' <h> ' ' <path>
//                                  preview region: native-pixel rect x,y,rw,rh of the
//                                  (upright) image, scaled to w x h
//                                  Only the newest preview request is kept.
// stdout, one record per request, all integers LE:
//   'O' | 'X', u32 len, path          thumbnail made / failed; 'O' adds u8 w, u8 h and
//                                     w*h premultiplied ARGB32 pixels (u32), fitted into
//                                     THUMB x THUMB keeping the aspect ratio
//   'P' | 'Q', u32 len, path, u32 id  preview made / not an image; 'P' adds u32 native
//                                     width and height, the native rect covered (u32 x,
//                                     y, w, h), u32 w, u32 h and w*h premultiplied
//                                     ARGB32 pixels

#include <KFileItem>
#include <KIO/PreviewJob>
#include <QFile>
#include <QGuiApplication>
#include <QImage>
#include <QImageReader>
#include <QPainter>
#include <QSocketNotifier>

#include <algorithm>
#include <bit>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <mutex>
#include <optional>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>

namespace {

constexpr int THUMB = 16;
// A standard freedesktop size, so KIO caches it where every app looks ("normal").
constexpr int GENERATE = 128;
constexpr int BATCH = 8;
constexpr int JOBS = 2;

// Thumbnails are written from the Qt thread, previews from the preview thread.
std::mutex g_out;

void writeU32(uint32_t v) {
    unsigned char b[4] = {uint8_t(v), uint8_t(v >> 8), uint8_t(v >> 16), uint8_t(v >> 24)};
    fwrite(b, 1, 4, stdout);
}

void writeHeader(char status, const QByteArray &path) {
    fputc(status, stdout);
    writeU32(uint32_t(path.size()));
    fwrite(path.constData(), 1, size_t(path.size()), stdout);
}

void writePixels(const QImage &img) {
    // ARGB32 scanlines are native-endian u32s; the protocol is LE, as is every host we run on.
    static_assert(std::endian::native == std::endian::little);
    for (int y = 0; y < img.height(); ++y) {
        fwrite(img.constScanLine(y), 4, size_t(img.width()), stdout);
    }
}

void made(const KFileItem &item, const QImage &image) {
    const QImage img = image
                           .scaled(THUMB, THUMB, Qt::KeepAspectRatio, Qt::SmoothTransformation)
                           .convertToFormat(QImage::Format_ARGB32_Premultiplied);
    const std::lock_guard lock(g_out);
    writeHeader('O', QFile::encodeName(item.url().toLocalFile()));
    fputc(img.width(), stdout);
    fputc(img.height(), stdout);
    writePixels(img);
    fflush(stdout);
}

void failed(const KFileItem &item) {
    const std::lock_guard lock(g_out);
    writeHeader('X', QFile::encodeName(item.url().toLocalFile()));
    fflush(stdout);
}

struct PreviewRequest {
    uint32_t id = 0;
    QByteArray path;
    // Fit: the box to fit into. Region: the native rect and the size to scale it to.
    bool region = false;
    QSize max;
    QRect rect;
    QSize size;
};

struct Preview {
    QImage img;
    QSize native;
    QRect covers;
};

// Reads the image; PDF pages come back transparent where the paper is, so they get the
// white page they'd have printed.
QImage readImage(QImageReader &reader) {
    QImage img = reader.read();
    if (img.isNull() || reader.format() != "pdf" || !img.hasAlphaChannel()) {
        return img;
    }
    QImage page(img.size(), QImage::Format_ARGB32_Premultiplied);
    page.fill(Qt::white);
    QPainter painter(&page);
    painter.drawImage(0, 0, img);
    painter.end();
    return page;
}

// The whole image fitted into `max` (never enlarged), upright per EXIF.
Preview decodeFit(const PreviewRequest &req) {
    QImageReader reader(QFile::decodeName(req.path));
    reader.setAutoTransform(true);
    const QSize src = reader.size();
    const bool turned = reader.transformation() & QImageIOHandler::TransformationRotate90;
    const QSize shown = turned ? src.transposed() : src;
    if (src.isValid()) {
        const double scale = std::min({double(req.max.width()) / shown.width(),
                                       double(req.max.height()) / shown.height(), 1.0});
        if (scale < 1.0 && reader.supportsOption(QImageIOHandler::ScaledSize)) {
            const QSize target = (QSizeF(shown) * scale).toSize().expandedTo(QSize(1, 1));
            // Formats like JPEG then decode at the smaller size directly.
            reader.setScaledSize(turned ? target.transposed() : target);
        }
    }
    QImage img = readImage(reader);
    if (img.isNull()) {
        return {};
    }
    const QSize native = src.isValid() ? shown : img.size();
    if (img.width() > req.max.width() || img.height() > req.max.height()) {
        img = img.scaled(req.max, Qt::KeepAspectRatio, Qt::SmoothTransformation);
    }
    return {img, native, QRect(QPoint(0, 0), native)};
}

// Decodes previews off the Qt thread so thumbnails keep flowing. Only the newest
// request matters: one arriving mid-decode replaces any still waiting.
class Previewer {
public:
    Previewer() : m_thread([this] { run(); }) {
        m_thread.detach();
    }

    void request(PreviewRequest req) {
        {
            const std::lock_guard lock(m_mutex);
            m_next = std::move(req);
        }
        m_wake.notify_one();
    }

private:
    // A region of the full-resolution image, kept decoded while the same file is zoomed.
    Preview decodeRegion(const PreviewRequest &req) {
        if (m_fullPath != req.path) {
            QImageReader reader(QFile::decodeName(req.path));
            reader.setAutoTransform(true);
            m_full = readImage(reader);
            m_fullPath = req.path;
        }
        if (m_full.isNull()) {
            return {};
        }
        const QRect rect = req.rect.intersected(m_full.rect());
        if (rect.isEmpty() || req.size.isEmpty()) {
            return {};
        }
        QImage img = m_full.copy(rect);
        if (img.size() != req.size) {
            img = img.scaled(req.size, Qt::IgnoreAspectRatio, Qt::SmoothTransformation);
        }
        return {img, m_full.size(), rect};
    }

    void run() {
        for (;;) {
            PreviewRequest req;
            {
                std::unique_lock lock(m_mutex);
                m_wake.wait(lock, [this] { return m_next.has_value(); });
                req = std::move(*m_next);
                m_next.reset();
            }
            Preview p = req.region ? decodeRegion(req) : decodeFit(req);
            if (!p.img.isNull()) {
                p.img = p.img.convertToFormat(QImage::Format_ARGB32_Premultiplied);
            }
            const std::lock_guard lock(g_out);
            writeHeader(p.img.isNull() ? 'Q' : 'P', req.path);
            writeU32(req.id);
            if (!p.img.isNull()) {
                for (int v : {p.native.width(), p.native.height(), p.covers.x(), p.covers.y(),
                              p.covers.width(), p.covers.height(), p.img.width(), p.img.height()}) {
                    writeU32(uint32_t(v));
                }
                writePixels(p.img);
            }
            fflush(stdout);
        }
    }

    std::mutex m_mutex;
    std::condition_variable m_wake;
    std::optional<PreviewRequest> m_next;
    QByteArray m_fullPath;
    QImage m_full;
    std::thread m_thread;
};

class Helper : public QObject {
public:
    Helper() : m_notifier(STDIN_FILENO, QSocketNotifier::Read), m_plugins(KIO::PreviewJob::availablePlugins()) {
        connect(&m_notifier, &QSocketNotifier::activated, this, &Helper::readInput);
    }

private:
    void readInput() {
        char tmp[65536];
        const ssize_t n = ::read(STDIN_FILENO, tmp, sizeof tmp);
        if (n <= 0) {
            // swayfin is gone. Exit right away: the preview thread may be mid-decode, and
            // nothing needs tearing down (KIO's caching is complete per thumbnail).
            fflush(stdout);
            ::_exit(0);
        }
        m_input.append(tmp, n);
        qsizetype end;
        while ((end = m_input.indexOf('\0')) >= 0) {
            const QByteArray record = m_input.left(end);
            m_input.remove(0, end + 1);
            if (record == "\x01") {
                cancel();
            } else if (record.startsWith('\x02')) {
                preview(record.mid(1));
            } else if (!record.isEmpty()) {
                m_queue.append(record);
            }
        }
        pump();
    }

    // "<id> F <maxw> <maxh> <path>" or "<id> R <x> <y> <rw> <rh> <w> <h> <path>"
    void preview(const QByteArray &args) {
        QList<QByteArray> f;
        qsizetype pos = 0;
        const int fields = args.mid(0, args.indexOf(' ') + 3).endsWith(" R ") ? 8 : 4;
        for (int i = 0; i < fields; ++i) {
            const qsizetype sp = args.indexOf(' ', pos);
            if (sp < 0) {
                return;
            }
            f.append(args.mid(pos, sp - pos));
            pos = sp + 1;
        }
        PreviewRequest req;
        req.id = f[0].toUInt();
        req.path = args.mid(pos);
        if (f[1] == "R") {
            req.region = true;
            req.rect = QRect(f[2].toInt(), f[3].toInt(), f[4].toInt(), f[5].toInt());
            req.size = QSize(f[6].toInt(), f[7].toInt());
        } else {
            req.max = QSize(f[2].toInt(), f[3].toInt());
            if (req.max.isEmpty()) {
                return;
            }
        }
        m_previewer.request(std::move(req));
    }

    void cancel() {
        m_queue.clear();
        for (KIO::PreviewJob *job : std::as_const(m_jobs)) {
            job->kill();
        }
        m_jobs.clear();
    }

    void pump() {
        while (m_jobs.size() < JOBS && !m_queue.isEmpty()) {
            KFileItemList items;
            while (items.size() < BATCH && !m_queue.isEmpty()) {
                const QUrl url = QUrl::fromLocalFile(QFile::decodeName(m_queue.takeFirst()));
                items.append(KFileItem(url, QString(), S_IFREG));
            }
            auto *job = new KIO::PreviewJob(items, QSize(GENERATE, GENERATE), &m_plugins);
            job->setScaleType(KIO::PreviewJob::ScaledAndCached);
            connect(job, &KIO::PreviewJob::generated, this, &made);
            connect(job, &KIO::PreviewJob::failed, this, &failed);
            connect(job, &KJob::result, this, [this, job] {
                m_jobs.removeOne(job);
                pump();
            });
            m_jobs.append(job);
            job->start();
        }
    }

    QSocketNotifier m_notifier;
    QStringList m_plugins;
    Previewer m_previewer;
    QByteArray m_input;
    QList<QByteArray> m_queue;
    QList<KIO::PreviewJob *> m_jobs;
};

} // namespace

int main(int argc, char **argv) {
    // Headless: never connect to the compositor.
    qputenv("QT_QPA_PLATFORM", "offscreen");
    // The session's platform theme (xdgdesktopportal, for file dialogs) would talk to
    // the portal at startup for nothing.
    qunsetenv("QT_QPA_PLATFORMTHEME");
    QGuiApplication app(argc, argv);
    // Finished KIO jobs release event-loop locks, which would otherwise quit the app
    // between batches. Only stdin closing ends the helper.
    QCoreApplication::setQuitLockEnabled(false);
    QGuiApplication::setQuitOnLastWindowClosed(false);
    Helper helper;
    return app.exec();
}
