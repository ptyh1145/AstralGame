// Astral Game —— iOS 原生界面（armv7 / iOS 8+）
//
// 为什么不是原版 Flutter：Flutter 的 iOS 引擎只有 arm64、最低 iOS 13，
// 在 iOS 8/armv7 上根本无法运行。所以这里用 UIKit 做一个精简界面，
// 联机核心复用已经跑通的 Rust（EasyTier + utun），通过 C ABI 调用。
//
// 界面元素：
//   房间名 / 密码 / 服务器地址 三个输入框
//   [连接] 按钮 + 状态标签 + 滚动日志
//   [复制日志] 按钮（方便把日志贴出来排查）

#import <UIKit/UIKit.h>
#include <time.h>
#include <stdlib.h>
#include <sys/time.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <string.h>
#include <stdio.h>

extern int g_uid_before;
extern int g_uid_after;

// ---------------------------------------------------------------- Rust C ABI

extern int astral_start(const char *room, const char *secret,
                        const char *ipv4, const char *peer);
extern int astral_stop(void);
extern unsigned char astral_state(void);
extern const char *astral_state_text(void);
extern const char *astral_log(void);
extern void astral_clear_log(void);
extern void astral_set_hostname(const char *name);

// -------------------------------------------- 老系统缺失符号（供 std/ring 用）
//
// clock_gettime：iOS 10 才有；std 的 Timespec::now 与 quanta 会调用它。
//                这里用 gettimeofday 等价实现。
// CCRandomGenerateBytes：std 的 HashMap 随机源；用 arc4random_buf 替代。

int clock_gettime(int clk_id, struct timespec *tp) {
    if (tp == NULL) return -1;
    // 只支持 REALTIME(0) / MONOTONIC(6)
    if (clk_id != 0 && clk_id != 6) return -1;
    struct timeval tv;
    if (gettimeofday(&tv, NULL) != 0) return -1;
    tp->tv_sec = tv.tv_sec;
    tp->tv_nsec = (long)tv.tv_usec * 1000;
    return 0;
}

int CCRandomGenerateBytes(void *bytes, size_t count) {
    if (bytes == NULL) return -1;
    arc4random_buf(bytes, count);
    return 0;
}

// ---------------------------------------------------------------- 界面

@interface AstralViewController : UIViewController <UITextFieldDelegate>
@property (nonatomic, strong) UITextField *codeField;
@property (nonatomic, strong) UIButton *joinButton;
@property (nonatomic, strong) UITextField *roomField;
@property (nonatomic, strong) UITextField *secretField;
@property (nonatomic, strong) UITextField *peerField;
@property (nonatomic, strong) UITextField *nickField;
@property (nonatomic, strong) UIButton *createButton;
@property (nonatomic, strong) UIButton *connectButton;
@property (nonatomic, strong) UILabel *stateLabel;
@property (nonatomic, strong) UITextView *logView;
@property (nonatomic, strong) NSTimer *pollTimer;
@end

@implementation AstralViewController

- (void)viewDidLoad {
    [super viewDidLoad];
    self.title = @"Astral Game";
    self.view.backgroundColor = [UIColor colorWithWhite:0.96 alpha:1.0];

    CGFloat w = self.view.bounds.size.width;
    CGFloat y = 12;

    // 房间码：官方短码服务（astral-share），输入 6 位码即可自动拉取房间配置
    self.codeField = [self makeField:@"房间码（6 位，可留空）" y:y]; y += 46;
    self.joinButton = [UIButton buttonWithType:UIButtonTypeSystem];
    self.joinButton.frame = CGRectMake(16, y, w - 32, 34);
    [self.joinButton setTitle:@"用房间码加入" forState:UIControlStateNormal];
    [self.joinButton addTarget:self action:@selector(joinByCode)
              forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:self.joinButton];
    y += 44;

    self.roomField = [self makeField:@"房间名" y:y]; y += 46;
    self.roomField.text = @"test";
    self.secretField = [self makeField:@"密码" y:y]; y += 46;
    self.secretField.text = @"123456";
    self.peerField = [self makeField:@"服务器 (tcp://ip:11010)" y:y]; y += 46;
    self.nickField = [self makeField:@"昵称（显示在房间里）" y:y]; y += 46;
    self.nickField.text = @"iPhone";

    CGFloat halfW = (w - 32 - 8) / 2;
    self.createButton = [UIButton buttonWithType:UIButtonTypeSystem];
    self.createButton.frame = CGRectMake(16, y, halfW, 44);
    [self.createButton setTitle:@"创建房间" forState:UIControlStateNormal];
    self.createButton.titleLabel.font = [UIFont boldSystemFontOfSize:16];
    [self.createButton addTarget:self action:@selector(createRoom)
                forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:self.createButton];

    self.connectButton = [UIButton buttonWithType:UIButtonTypeSystem];
    self.connectButton.frame = CGRectMake(16 + halfW + 8, y, halfW, 44);
    [self.connectButton setTitle:@"连接" forState:UIControlStateNormal];
    self.connectButton.titleLabel.font = [UIFont boldSystemFontOfSize:18];
    [self.connectButton addTarget:self action:@selector(toggleConnect)
                 forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:self.connectButton];
    y += 52;

    self.stateLabel = [[UILabel alloc] initWithFrame:CGRectMake(16, y, w - 32, 24)];
    self.stateLabel.font = [UIFont systemFontOfSize:14];
    self.stateLabel.textColor = [UIColor darkGrayColor];
    self.stateLabel.text = @"状态：未连接";
    [self.view addSubview:self.stateLabel];
    y += 28;

    self.logView = [[UITextView alloc] initWithFrame:CGRectMake(16, y, w - 32,
                                    self.view.bounds.size.height - y - 56)];
    self.logView.editable = NO;
    self.logView.font = [UIFont fontWithName:@"Menlo" size:11] ?: [UIFont systemFontOfSize:11];
    self.logView.backgroundColor = [UIColor whiteColor];
    {
        char buf[256];
        snprintf(buf, sizeof(buf),
                 "uid: %d -> %d\n(如果 after 是 0，说明提权成功，utun 可以建)\n\n",
                 g_uid_before, g_uid_after);
        self.logView.text = [NSString stringWithUTF8String:buf];
    }
    [self.view addSubview:self.logView];

    UIButton *copyBtn = [UIButton buttonWithType:UIButtonTypeSystem];
    copyBtn.frame = CGRectMake(w - 140, self.view.bounds.size.height - 44, 130, 32);
    [copyBtn setTitle:@"复制日志" forState:UIControlStateNormal];
    [copyBtn addTarget:self action:@selector(copyLog) forControlEvents:UIControlEventTouchUpInside];
    [self.view addSubview:copyBtn];

    // iOS 8 兼容：用 NSTimer 轮询，而不是 CADisplayLink / async
    self.pollTimer = [NSTimer scheduledTimerWithTimeInterval:0.5
                                                      target:self
                                                    selector:@selector(poll)
                                                    userInfo:nil
                                                     repeats:YES];
}

- (UITextField *)makeField:(NSString *)placeholder y:(CGFloat)y {
    CGFloat w = self.view.bounds.size.width;
    UITextField *f = [[UITextField alloc] initWithFrame:CGRectMake(16, y, w - 32, 40)];
    f.placeholder = placeholder;
    f.borderStyle = UITextBorderStyleRoundedRect;
    f.autocorrectionType = UITextAutocorrectionTypeNo;
    f.autocapitalizationType = UITextAutocapitalizationTypeNone;
    f.clearButtonMode = UITextFieldViewModeWhileEditing;
    f.returnKeyType = UIReturnKeyDone;
    f.delegate = self;
    f.text = @"";
    [self.view addSubview:f];
    return f;
}

- (BOOL)textFieldShouldReturn:(UITextField *)textField {
    [textField resignFirstResponder];
    return YES;
}

- (void)poll {
    unsigned char st = (unsigned char)[self currentState];
    NSString *s = @"?";
    {
        NSString *resp = [self sockSend:@"STATUS"];
        if (resp) {
            for (NSString *ln in [resp componentsSeparatedByString:@"\n"]) {
                if ([ln hasPrefix:@"text="]) {
                    s = [ln substringFromIndex:5];
                    break;
                }
            }
        } else {
            const char *txt = astral_state_text();
            s = [NSString stringWithUTF8String:(txt ? txt : "?")];
        }
    }
    NSString *prefix = nil;
    switch (st) {
        case 0: prefix = @"未连接"; break;
        case 1: prefix = @"连接中…"; break;
        case 2: prefix = @"已连接"; break;
        case 3: prefix = @"出错"; break;
        default: prefix = @"?"; break;
    }
    self.stateLabel.text = [NSString stringWithFormat:@"状态：%@ (%@)", prefix, s];
    self.stateLabel.textColor = (st == 3) ? [UIColor redColor]
                                : (st == 2 ? [UIColor colorWithRed:0 green:0.5 blue:0 alpha:1]
                                           : [UIColor darkGrayColor]);

    [self.connectButton setTitle:(st == 2 ? @"断开" : @"连接") forState:UIControlStateNormal];

    NSString *full = [self sockSend:@"LOG"];
    if (!full) {
        const char *lg = astral_log();
        full = lg ? [NSString stringWithUTF8String:lg] : nil;
    }
    if (full.length > 0 && ![full isEqualToString:self.logView.text]) {
        self.logView.text = full;
        [self.logView scrollRangeToVisible:NSMakeRange(full.length, 0)];
    }
}

// 短码服务：GET http://103.194.107.25:8080/v1/codes/{6位码}
// 返回 {"network_name","network_secret","peers":[{"uri":"tcp://..."}],...}
// 这就是官方"房间码"联机用的服务。
- (NSString *)normalizeCode:(NSString *)raw {
    NSMutableString *b = [NSMutableString string];
    NSString *up = [(raw ?: @"") uppercaseString];
    for (NSUInteger i = 0; i < up.length; i++) {
        unichar c = [up characterAtIndex:i];
        if ((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z')) {
            [b appendFormat:@"%C", c];
        }
    }
    return b;
}

- (void)joinByCode {
    [self.view endEditing:YES];
    NSString *code = [self normalizeCode:self.codeField.text];
    if (code.length != 6) {
        [self alert:@"房间码应为 6 位字母数字"];
        return;
    }
    self.stateLabel.text = @"正在查询房间码…";
    NSString *urlStr = [NSString stringWithFormat:
                        @"http://103.194.107.25:8080/v1/codes/%@", code];
    NSURL *url = [NSURL URLWithString:urlStr];
    if (!url) { [self alert:@"房间码格式错误"]; return; }

    // MRC 下不能用 __weak；block 捕获也不会 retain，不存在循环引用
    AstralViewController *me = self;
    NSURLSessionDataTask *task =
        [[NSURLSession sharedSession] dataTaskWithURL:url
            completionHandler:^(NSData *data, NSURLResponse *resp, NSError *err) {
        dispatch_async(dispatch_get_main_queue(), ^{
            if (!me) return;
            if (err) {
                me.stateLabel.text = @"房间码服务连接失败";
                [me alert:[NSString stringWithFormat:@"网络错误：%@",
                           err.localizedDescription]];
                return;
            }
            NSInteger status = [(NSHTTPURLResponse *)resp statusCode];
            if (status == 404) { [me alert:@"房间码无效或已过期"]; return; }
            if (status == 429) { [me alert:@"查询过于频繁，请稍后再试"]; return; }
            if (status < 200 || status >= 300) {
                [me alert:[NSString stringWithFormat:@"短码服务错误 (%ld)",
                           (long)status]];
                return;
            }
            NSDictionary *j = [NSJSONSerialization JSONObjectWithData:data
                                                             options:0
                                                               error:NULL];
            if (![j isKindOfClass:[NSDictionary class]]) {
                [me alert:@"房间码返回格式异常"]; return;
            }
            NSString *name = j[@"network_name"];
            NSString *secret = j[@"network_secret"];
            NSArray *peers = j[@"peers"];
            NSString *uri = @"";
            if ([peers isKindOfClass:[NSArray class]] && peers.count > 0) {
                id p0 = peers[0];
                if ([p0 isKindOfClass:[NSDictionary class]]) {
                    uri = p0[@"uri"] ?: @"";
                }
            }
            if (name.length == 0 || secret.length == 0) {
                [me alert:@"房间码载荷不完整（缺少房间名/密码）"]; return;
            }
            // 回填到界面，便于用户看到实际配置
            me.roomField.text = name;
            me.secretField.text = secret;
            me.peerField.text = uri;
            me.stateLabel.text = [NSString stringWithFormat:
                @"已解析房间「%@」，服务器 %@", name,
                uri.length ? uri : @"(空，仅靠已有节点)"];
            [me startWithRoom:name secret:secret peer:uri];
        });
    }];
    [task resume];
}

// 与 root 守护进程通信：/var/run/astral.sock
// 守护进程能做 App 做不到的事（建 utun 需要 root）。连不上就返回 nil，
// 调用方回退到直接调 Rust 库（setuid 成功的场景）。
- (NSString *)sockSend:(NSString *)cmd {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return nil;
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, "/var/run/astral.sock", sizeof(addr.sun_path) - 1);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        close(fd);
        return nil;
    }
    NSString *line = [cmd stringByAppendingString:@"\n"];
    const char *bytes = [line UTF8String];
    write(fd, bytes, strlen(bytes));

    NSMutableData *out = [NSMutableData data];
    char buf[65536];
    ssize_t n;
    while ((n = read(fd, buf, sizeof(buf))) > 0) {
        [out appendBytes:buf length:(NSUInteger)n];
    }
    close(fd);
    return [[NSString alloc] initWithData:out encoding:NSUTF8StringEncoding];
}

// 创建房间：POST /v1/codes 换取 6 位房间码
// 请求体就是官方的 RoomInvitePayload，peers 填我们连的服务器地址。
- (void)createRoom {
    [self.view endEditing:YES];
    NSString *room = self.roomField.text ?: @"";
    NSString *secret = self.secretField.text ?: @"";
    NSString *peer = self.peerField.text ?: @"";
    NSString *nick = self.nickField.text ?: @"";
    peer = [peer stringByTrimmingCharactersInSet:
            [NSCharacterSet whitespaceAndNewlineCharacterSet]];
    if (room.length == 0 || secret.length == 0) {
        [self alert:@"房间名和密码不能为空"];
        return;
    }

    NSMutableArray *peerArr = [NSMutableArray array];
    if (peer.length > 0) {
        [peerArr addObject:@{@"uri": peer}];
    }
    NSDictionary *payload = @{
        @"v": @1,
        @"game_id": @"minecraft",
        @"game_name": @"\u6211\u7684\u4e16\u754c",
        @"network_name": room,
        @"network_secret": secret,
        @"peers": peerArr,
        @"display_name": nick,
        @"transport": @"easytier"
    };
    NSData *body = [NSJSONSerialization dataWithJSONObject:payload options:0 error:NULL];
    NSMutableURLRequest *req = [NSMutableURLRequest requestWithURL:
        [NSURL URLWithString:@"http://103.194.107.25:8080/v1/codes"]];
    [req setHTTPMethod:@"POST"];
    [req setValue:@"application/json" forHTTPHeaderField:@"Content-Type"];
    [req setHTTPBody:body];
    [req setTimeoutInterval:8];

    self.stateLabel.text = @"正在创建房间…";
    AstralViewController *me = self;
    NSURLSessionDataTask *task =
        [[NSURLSession sharedSession] dataTaskWithRequest:req
            completionHandler:^(NSData *data, NSURLResponse *resp, NSError *err) {
        dispatch_async(dispatch_get_main_queue(), ^{
            if (err) {
                me.stateLabel.text = @"创建房间失败";
                [me alert:[NSString stringWithFormat:@"网络错误：%@",
                           err.localizedDescription]];
                return;
            }
            NSInteger status = [(NSHTTPURLResponse *)resp statusCode];
            if (status == 429) { [me alert:@"创建过于频繁，请稍后再试"]; return; }
            if (status < 200 || status >= 300) {
                [me alert:[NSString stringWithFormat:@"短码服务错误 (%ld)", (long)status]];
                return;
            }
            NSDictionary *j = [NSJSONSerialization JSONObjectWithData:data
                                                             options:0 error:NULL];
            NSString *code = j[@"code"];
            if (![code isKindOfClass:[NSString class]] || code.length == 0) {
                [me alert:@"短码服务返回异常"];
                return;
            }
            [UIPasteboard generalPasteboard].string = code;
            me.codeField.text = code;
            [me alert:[NSString stringWithFormat:
                       @"房间码：%@\n（已复制到剪贴板，发给朋友即可）", code]];
            // 建完直接连上
            [me startWithRoom:room secret:secret peer:peer];
        });
    }];
    [task resume];
}

- (void)alert:(NSString *)msg {
    UIAlertView *a = [[UIAlertView alloc] initWithTitle:@"提示"
                                                message:msg
                                               delegate:nil
                                      cancelButtonTitle:@"好"
                                      otherButtonTitles:nil];
    [a show];
}

- (void)startWithRoom:(NSString *)room secret:(NSString *)secret peer:(NSString *)peer {
    astral_clear_log();
    NSString *ipv4 = @"10.144.144.5/24";

    // 昵称 → EasyTier hostname（房间成员列表里显示）
    NSString *nick = self.nickField.text ?: @"";
    nick = [nick stringByTrimmingCharactersInSet:
            [NSCharacterSet whitespaceAndNewlineCharacterSet]];
    if (nick.length > 0) {
        if (![self sockSend:[NSString stringWithFormat:@"HOST %@", nick]]) {
            astral_set_hostname([nick UTF8String]);
        }
    }

    // 优先让 root 守护进程干活（它能建 utun）
    NSString *resp = [self sockSend:[NSString stringWithFormat:
                        @"START %@ %@ %@ %@", room, secret, ipv4, peer]];
    if (resp) {
        if (![resp hasPrefix:@"rc=0"]) {
            [self alert:[NSString stringWithFormat:@"守护进程拒绝：%@", resp]];
        }
        return;
    }

    // 没有守护进程：退回直连（只有 App 自己是 root 时才可能成功）
    int rc = astral_start([room UTF8String], [secret UTF8String],
                          [ipv4 UTF8String], [peer UTF8String]);
    if (rc != 0) {
        [self alert:@"启动失败：既没有 root 守护进程，App 自己也不是 root"];
    }
}

// 读当前连接状态：socket 优先，退回 C ABI
- (NSInteger)currentState {
    NSString *resp = [self sockSend:@"STATUS"];
    if (resp) {
        for (NSString *ln in [resp componentsSeparatedByString:@"\n"]) {
            if ([ln hasPrefix:@"state="]) {
                return [[ln substringFromIndex:6] integerValue];
            }
        }
        return 0;
    }
    return (NSInteger)astral_state();
}

- (void)toggleConnect {
    [self.view endEditing:YES];

    // 只有「已连接(2)」才当断开处理，否则一律去连接
    if ([self currentState] == 2) {
        if ([self sockSend:@"STOP"]) return;
        astral_stop();
        return;
    }

    NSString *peer = self.peerField.text ?: @"";
    peer = [peer stringByTrimmingCharactersInSet:
            [NSCharacterSet whitespaceAndNewlineCharacterSet]];
    [self startWithRoom:(self.roomField.text ?: @"")
                 secret:(self.secretField.text ?: @"")
                   peer:peer];
}

- (void)copyLog {
    const char *lg = astral_log();
    NSString *s = lg ? [NSString stringWithUTF8String:lg] : @"";
    [UIPasteboard generalPasteboard].string = s;
    self.stateLabel.text = @"日志已复制到剪贴板";
}

@end

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@property (nonatomic, strong) UIWindow *window;
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application
        didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    self.window = [[UIWindow alloc] initWithFrame:[[UIScreen mainScreen] bounds]];
    AstralViewController *vc = [[AstralViewController alloc] init];
    UINavigationController *nav = [[UINavigationController alloc] initWithRootViewController:vc];
    self.window.rootViewController = nav;
    [self.window makeKeyAndVisible];
    return YES;
}

@end

// 越狱设备上尝试把自身提到 root：
//   /Applications 在 rootfs（非 nosuid），若二进制有 setuid 位，
//   SpringBoard 启动后这里 setuid(0) 能成功。
//   但提权后能否继续连 WindowServer 要实测（SpringBoard 只认 mobile）。
int g_uid_before = -1;
int g_uid_after = -1;

int main(int argc, char *argv[]) {
    @autoreleasepool {
        g_uid_before = (int)geteuid();
        if (g_uid_before != 0) {
            setgid(0);
            setuid(0);
        }
        g_uid_after = (int)geteuid();

        // 同时写到文件，方便 ssh 直接看（不依赖 UI/Console）
        FILE *f = fopen("/tmp/astral-uid.txt", "w");
        if (f) {
            fprintf(f, "before=%d after=%d uid=%d gid=%d\n",
                    g_uid_before, g_uid_after, (int)getuid(), (int)getgid());
            fclose(f);
        }
        return UIApplicationMain(argc, argv, nil, @"AppDelegate");
    }
}
